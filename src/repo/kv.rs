use super::{Column, DataStore, PinModeRequirement};
use crate::error::Error;
use crate::repo::{PinKind, PinMode, PinStore, References};
use async_trait::async_trait;
use futures::stream::{StreamExt, TryStreamExt};
use libipld::cid::Cid;
use once_cell::sync::OnceCell;
use sled::{
    self,
    transaction::{
        ConflictableTransactionError, TransactionError, TransactionResult, TransactionalTree,
        UnabortableTransactionError,
    },
    Config as DbConfig, Db, Mode as DbMode,
};
use std::collections::BTreeSet;
use std::convert::Infallible;
use std::path::PathBuf;
use std::str::{self, FromStr};

/// [`sled`] based pinstore implementation. Implements datastore which errors for each call.
/// Currently feature-gated behind `sled_data_store` feature in the [`crate::Types`], usable
/// directly in custom type configurations.
///
/// Current schema is to use the the default tree for storing pins, which are serialized as
/// [`get_pin_key`]. Depending on the kind of pin values are generated by [`direct_value`],
/// [`recursive_value`], and [`indirect_value`].
///
/// [`sled`]: https://github.com/spacejam/sled
#[derive(Debug)]
pub struct KvDataStore {
    path: PathBuf,
    // it is a trick for not modifying the Data:init
    db: OnceCell<Db>,
}

impl KvDataStore {
    pub fn new(root: PathBuf) -> KvDataStore {
        KvDataStore {
            path: root,
            db: Default::default(),
        }
    }

    fn get_db(&self) -> &Db {
        self.db.get().unwrap()
    }
}

#[async_trait]
impl DataStore for KvDataStore {
    async fn init(&self) -> Result<(), Error> {
        let config = DbConfig::new();

        let db = config
            .mode(DbMode::HighThroughput)
            .path(self.path.as_path())
            .open()?;

        match self.db.set(db) {
            Ok(()) => Ok(()),
            Err(_) => Err(anyhow::anyhow!("failed to init sled")),
        }
    }

    async fn open(&self) -> Result<(), Error> {
        Ok(())
    }

    /// Checks if a key is present in the datastore.
    async fn contains(&self, _col: Column, _key: &[u8]) -> Result<bool, Error> {
        Err(anyhow::anyhow!("not implemented"))
    }

    /// Returns the value associated with a key from the datastore.
    async fn get(&self, _col: Column, _key: &[u8]) -> Result<Option<Vec<u8>>, Error> {
        Err(anyhow::anyhow!("not implemented"))
    }

    /// Puts the value under the key in the datastore.
    async fn put(&self, _col: Column, _key: &[u8], _value: &[u8]) -> Result<(), Error> {
        Err(anyhow::anyhow!("not implemented"))
    }

    /// Removes a key-value pair from the datastore.
    async fn remove(&self, _col: Column, _key: &[u8]) -> Result<(), Error> {
        Err(anyhow::anyhow!("not implemented"))
    }

    /// Wipes the datastore.
    async fn wipe(&self) {
    }
}

// in the transactional parts of the [`Infallible`] is used to signal there is no additional
// custom error, not that the transaction was infallible in itself.

#[async_trait]
impl PinStore for KvDataStore {
    async fn is_pinned(&self, cid: &Cid) -> Result<bool, Error> {
        let cid = cid.to_owned();
        let db = self.get_db().to_owned();
        let span = tracing::Span::current();
        tokio::task::spawn_blocking(move || {
            let span = tracing::trace_span!(parent: &span, "blocking");
            let _g = span.enter();
            Ok(db.transaction::<_, _, Infallible>(|tree| {
                Ok(get_pinned_mode(tree, &cid)?.is_some())
            })?)
        })
        .await?
    }

    async fn insert_direct_pin(&self, target: &Cid) -> Result<(), Error> {
        use ConflictableTransactionError::Abort;
        let target = target.to_owned();
        let db = self.get_db().to_owned();

        let span = tracing::Span::current();

        let res = tokio::task::spawn_blocking(move || {
            let span = tracing::trace_span!(parent: &span, "blocking");
            let _g = span.enter();

            db.transaction(|tx_tree| {
                let already_pinned = get_pinned_mode(tx_tree, &target)?;

                match already_pinned {
                    Some((PinMode::Direct, _)) => return Ok(()),
                    Some((PinMode::Recursive, _)) => {
                        return Err(Abort(anyhow::anyhow!("already pinned recursively")))
                    }
                    Some((PinMode::Indirect, key)) => {
                        // TODO: I think the direct should live alongside the indirect?
                        tx_tree.remove(key.as_str())?;
                    }
                    None => {}
                }

                let direct_key = get_pin_key(&target, &PinMode::Direct);
                tx_tree.insert(direct_key.as_str(), direct_value())?;

                tx_tree.flush();

                Ok(())
            })
        })
        .await?;

        launder(res)
    }

    async fn insert_recursive_pin(
        &self,
        target: &Cid,
        referenced: References<'_>,
    ) -> Result<(), Error> {
        // since the transaction can be retried multiple times, we need to collect these and keep
        // iterating it until there is no conflict.
        let set = referenced.try_collect::<BTreeSet<_>>().await?;

        let target = target.to_owned();
        let db = self.get_db().to_owned();

        let span = tracing::Span::current();

        // the transaction is not infallible but there is no additional error we return
        tokio::task::spawn_blocking(move || {
            let span = tracing::trace_span!(parent: &span, "blocking");
            let _g = span.enter();
            db.transaction::<_, _, Infallible>(move |tx_tree| {
                let already_pinned = get_pinned_mode(tx_tree, &target)?;

                match already_pinned {
                    Some((PinMode::Recursive, _)) => return Ok(()),
                    Some((PinMode::Direct, key)) | Some((PinMode::Indirect, key)) => {
                        // FIXME: this is probably another lapse in tests that both direct and
                        // indirect can be removed when inserting recursive?
                        tx_tree.remove(key.as_str())?;
                    }
                    None => {}
                }

                let recursive_key = get_pin_key(&target, &PinMode::Recursive);
                tx_tree.insert(recursive_key.as_str(), recursive_value())?;

                let target_value = indirect_value(&target);

                // cannot use into_iter here as the transactions are retryable
                for cid in set.iter() {
                    let indirect_key = get_pin_key(cid, &PinMode::Indirect);

                    if matches!(get_pinned_mode(tx_tree, cid)?, Some(_)) {
                        // TODO: quite costly to do the get_pinned_mode here
                        continue;
                    }

                    // value is for get information like "Qmd9WDTA2Kph4MKiDDiaZdiB4HJQpKcxjnJQfQmM5rHhYK indirect through QmXr1XZBg1CQv17BPvSWRmM7916R6NLL7jt19rhCPdVhc5"
                    // FIXME: this will not work with multiple blocks linking to the same block? also the
                    // test is probably missing as well
                    tx_tree.insert(indirect_key.as_str(), target_value.as_str())?;
                }

                tx_tree.flush();
                Ok(())
            })
        })
        .await??;

        Ok(())
    }

    async fn remove_direct_pin(&self, target: &Cid) -> Result<(), Error> {
        use ConflictableTransactionError::Abort;
        let target = target.to_owned();
        let db = self.get_db().to_owned();

        let span = tracing::Span::current();

        let res = tokio::task::spawn_blocking(move || {
            let span = tracing::trace_span!(parent: &span, "blocking");
            let _g = span.enter();

            db.transaction::<_, _, Error>(|tx_tree| {
                if is_not_pinned_or_pinned_indirectly(tx_tree, &target)? {
                    return Err(Abort(anyhow::anyhow!("not pinned or pinned indirectly")));
                }

                let key = get_pin_key(&target, &PinMode::Direct);
                tx_tree.remove(key.as_str())?;
                tx_tree.flush();
                Ok(())
            })
        })
        .await?;

        launder(res)
    }

    async fn remove_recursive_pin(
        &self,
        target: &Cid,
        referenced: References<'_>,
    ) -> Result<(), Error> {
        use ConflictableTransactionError::Abort;
        // TODO: is this "in the same transaction" as the batch which is created?
        let set = referenced.try_collect::<BTreeSet<_>>().await?;

        let target = target.to_owned();
        let db = self.get_db().to_owned();

        let span = tracing::Span::current();

        let res = tokio::task::spawn_blocking(move || {
            let span = tracing::trace_span!(parent: &span, "blocking");
            let _g = span.enter();

            db.transaction(|tx_tree| {
                if is_not_pinned_or_pinned_indirectly(tx_tree, &target)? {
                    return Err(Abort(anyhow::anyhow!("not pinned or pinned indirectly")));
                }

                let recursive_key = get_pin_key(&target, &PinMode::Recursive);
                tx_tree.remove(recursive_key.as_str())?;

                for cid in &set {
                    let already_pinned = get_pinned_mode(tx_tree, cid)?;

                    match already_pinned {
                        Some((PinMode::Recursive, _)) | Some((PinMode::Direct, _)) => continue, // this should be unreachable
                        Some((PinMode::Indirect, key)) => {
                            // FIXME: not really sure of this but it might be that recursive removed
                            // the others...?
                            tx_tree.remove(key.as_str())?;
                        }
                        None => {}
                    }
                }

                tx_tree.flush();
                Ok(())
            })
        })
        .await?;

        launder(res)
    }

    async fn list(
        &self,
        requirement: Option<PinMode>,
    ) -> futures::stream::BoxStream<'static, Result<(Cid, PinMode), Error>> {
        use tokio_stream::wrappers::UnboundedReceiverStream;

        let db = self.get_db().to_owned();

        // if the pins are always updated in transaction, we might get away with just tree reads.
        // this does however mean that it is possible to witness for example a part of a larger
        // recursive pin and then just not find anymore of the recursive pin near the end of the
        // listing. for non-gc uses this should not be an issue.
        //
        // FIXME: the unboundedness is still quite unoptimal here: we might get gazillion http
        // listings which all quickly fill up a lot of memory and clients never have to read any
        // responses. using of bounded channel would require sometimes sleeping and maybe bouncing
        // back and forth between an async task and continuation of the iteration. leaving this to
        // a later issue.
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();

        let span = tracing::Span::current();

        let _jh = tokio::task::spawn_blocking(move || {
            let span = tracing::trace_span!(parent: &span, "blocking");
            let _g = span.enter();

            // this probably doesn't need to be transactional? well, perhaps transactional reads would
            // be the best, not sure what is the guaratee for in-sequence key reads.
            let iter = db.range::<String, std::ops::RangeFull>(..);

            let requirement = PinModeRequirement::from(requirement);

            let adapted =
                iter.map(|res| res.map_err(Error::from))
                    .filter_map(move |res| match res {
                        Ok((k, _v)) => {
                            if !k.starts_with(b"pin.") || k.len() < 7 {
                                return Some(Err(anyhow::anyhow!(
                                    "invalid pin: {:?}",
                                    &*String::from_utf8_lossy(&k)
                                )));
                            }

                            let mode = match k[4] {
                                b'd' => PinMode::Direct,
                                b'r' => PinMode::Recursive,
                                b'i' => PinMode::Indirect,
                                x => {
                                    return Some(Err(anyhow::anyhow!(
                                        "invalid pinmode: {}",
                                        x as char
                                    )))
                                }
                            };

                            if !requirement.matches(&mode) {
                                None
                            } else {
                                let cid = std::str::from_utf8(&k[6..]).map_err(Error::from);
                                let cid = cid.and_then(|x| Cid::from_str(x).map_err(Error::from));
                                let cid = cid.map_err(|e| {
                                    e.context(format!(
                                        "failed to read pin: {:?}",
                                        &*String::from_utf8_lossy(&k)
                                    ))
                                });
                                Some(cid.map(move |cid| (cid, mode)))
                            }
                        }
                        Err(e) => Some(Err(e)),
                    });

            for res in adapted {
                if tx.send(res).is_err() {
                    break;
                }
            }
        });

        // we cannot know if the task was spawned successfully until it has completed, so we cannot
        // really do anything with the _jh.
        //
        // perhaps we could await for the first element OR cancellation OR perhaps something
        // else. StreamExt::peekable() would be good to go, but Peekable is only usable on top of
        // something pinned, and I cannot see how could it become a boxed stream if we pin it, peek
        // it and ... how would we get the peeked element since Peekable::into_inner doesn't return
        // the value which has already been read from the stream?
        //
        // it would be nice to make sure that the stream doesn't end before task has ended, but
        // perhaps the unboundedness of the channel takes care of that.
        UnboundedReceiverStream::new(rx).boxed()
    }

    async fn query(
        &self,
        ids: Vec<Cid>,
        requirement: Option<PinMode>,
    ) -> Result<Vec<(Cid, PinKind<Cid>)>, Error> {
        use ConflictableTransactionError::Abort;
        let requirement = PinModeRequirement::from(requirement);

        let db = self.get_db().to_owned();

        tokio::task::spawn_blocking(move || {
            let res = db.transaction::<_, _, Error>(|tx_tree| {
                // since its an Fn closure this cannot be reserved once ... not sure why it couldn't be
                // FnMut? the vec could be cached in the "outer" scope in a refcell.
                let mut modes = Vec::with_capacity(ids.len());

                // as we might loop over an over on the tx we might need this over and over, cannot
                // take ownership inside the transaction. TODO: perhaps the use of transaction is
                // questionable here; if the source of the indirect pin cannot be it is already
                // None, this could work outside of transaction similarly.
                for id in ids.iter() {
                    let mode_and_key = get_pinned_mode(tx_tree, id)?;

                    let matched = match mode_and_key {
                        Some((pin_mode, key)) if requirement.matches(&pin_mode) => match pin_mode {
                            PinMode::Direct => Some(PinKind::Direct),
                            PinMode::Recursive => Some(PinKind::Recursive(0)),
                            PinMode::Indirect => tx_tree
                                .get(key.as_str())?
                                .map(|root| {
                                    cid_from_indirect_value(&root)
                                        .map(PinKind::IndirectFrom)
                                        .map_err(|e| {
                                            Abort(e.context(format!(
                                                "failed to read indirect pin source: {:?}",
                                                String::from_utf8_lossy(root.as_ref()).as_ref(),
                                            )))
                                        })
                                })
                                .transpose()?,
                        },
                        Some(_) | None => None,
                    };

                    // this might be None, or Some(PinKind); it's important there are as many cids
                    // as there are modes
                    modes.push(matched);
                }

                Ok(modes)
            });

            let modes = launder(res)?;

            Ok(ids
                .into_iter()
                .zip(modes.into_iter())
                .filter_map(|(cid, mode)| mode.map(move |mode| (cid, mode)))
                .collect::<Vec<_>>())
        })
        .await?
    }
}

/// Name the empty value stored for direct pins; the pin key itself describes the mode and the cid.
fn direct_value() -> &'static [u8] {
    Default::default()
}

/// Name the empty value stored for recursive pins at the top.
fn recursive_value() -> &'static [u8] {
    Default::default()
}

/// Name the value stored for indirect pins, currently only the most recent recursive pin.
fn indirect_value(recursively_pinned: &Cid) -> String {
    recursively_pinned.to_string()
}

/// Inverse of [`indirect_value`].
fn cid_from_indirect_value(bytes: &[u8]) -> Result<Cid, Error> {
    str::from_utf8(bytes)
        .map_err(Error::from)
        .and_then(|s| Cid::from_str(s).map_err(Error::from))
}

/// Helper needed as the error cannot just `?` converted.
fn launder<T>(res: TransactionResult<T, Error>) -> Result<T, Error> {
    use TransactionError::*;
    match res {
        Ok(t) => Ok(t),
        Err(Abort(e)) => Err(e),
        Err(Storage(e)) => Err(e.into()),
    }
}

fn pin_mode_literal(pin_mode: &PinMode) -> &'static str {
    match pin_mode {
        PinMode::Direct => "d",
        PinMode::Indirect => "i",
        PinMode::Recursive => "r",
    }
}

fn get_pin_key(cid: &Cid, pin_mode: &PinMode) -> String {
    // TODO: get_pinned_mode could be range query if the pin modes were suffixes, keys would need
    // to be cid.to_bytes().push(pin_mode_literal(pin_mode))? ... since the cid bytes
    // representation already contains the length we should be good to go in all cases.
    //
    // for storing multiple targets then the last could be found by doing a query as well. in the
    // case of multiple indirect pins they'd have to be with another suffix.
    //
    // TODO: check if such representation would really order properly
    format!("pin.{}.{}", pin_mode_literal(pin_mode), cid)
}

/// Returns a tuple of the parsed mode and the key used
fn get_pinned_mode(
    tree: &TransactionalTree,
    block: &Cid,
) -> Result<Option<(PinMode, String)>, UnabortableTransactionError> {
    for mode in &[PinMode::Direct, PinMode::Recursive, PinMode::Indirect] {
        let key = get_pin_key(block, mode);

        if tree.get(key.as_str())?.is_some() {
            return Ok(Some((*mode, key)));
        }
    }

    Ok(None)
}

fn is_not_pinned_or_pinned_indirectly(
    tree: &TransactionalTree,
    block: &Cid,
) -> Result<bool, UnabortableTransactionError> {
    match get_pinned_mode(tree, block)? {
        Some((PinMode::Indirect, _)) | None => Ok(true),
        _ => Ok(false),
    }
}

#[cfg(test)]
crate::pinstore_interface_tests!(common_tests, crate::repo::kv::KvDataStore::new);
