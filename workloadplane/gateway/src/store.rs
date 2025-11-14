use std::borrow::Cow;

use libp2p::{
    kad::record::{Key, ProviderRecord, Record},
    kad::record::store::{self, RecordStore},
    kad::RecordKey,
    PeerId,
};

#[derive(Clone)]
pub struct LocalProviderStore {
    peer_id: PeerId,
    key: RecordKey,
    provider: Option<ProviderRecord>,
}

impl LocalProviderStore {
    pub fn new(peer_id: PeerId, key: RecordKey) -> Self {
        Self {
            peer_id,
            key,
            provider: None,
        }
    }
}

impl RecordStore for LocalProviderStore {
    type RecordsIter<'a> = std::iter::Empty<Cow<'a, Record>> where Self: 'a;
    type ProvidedIter<'a> = std::vec::IntoIter<Cow<'a, ProviderRecord>> where Self: 'a;

    fn get(&self, _k: &Key) -> Option<Cow<'_, Record>> {
        None
    }

    fn put(&mut self, _r: Record) -> store::Result<()> {
        Ok(())
    }

    fn remove(&mut self, _k: &Key) {}

    fn records(&self) -> Self::RecordsIter<'_> {
        std::iter::empty()
    }

    fn add_provider(&mut self, record: ProviderRecord) -> store::Result<()> {
        if record.provider != self.peer_id || record.key != self.key {
            return Ok(());
        }
        self.provider = Some(record);
        Ok(())
    }

    fn providers(&self, key: &Key) -> Vec<ProviderRecord> {
        if key == &self.key {
            self.provider.iter().cloned().collect()
        } else {
            Vec::new()
        }
    }

    fn provided(&self) -> Self::ProvidedIter<'_> {
        let mut entries = Vec::new();
        if let Some(record) = &self.provider {
            entries.push(Cow::Owned(record.clone()));
        }
        entries.into_iter()
    }

    fn remove_provider(&mut self, key: &Key, peer: &PeerId) {
        if peer == &self.peer_id && key == &self.key {
            self.provider = None;
        }
    }
}
