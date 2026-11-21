//! Chat contact models and CRDT sync utilities.

use std::{collections::HashMap, sync::Arc};

use anyhow::Result;
use autosurgeon::{Hydrate, Reconcile};

use crate::{
    helpers::{
        async_utils::now_epoch_millis,
        capnp::{capnp_enum_convert, capnp_u32, pack, unpack},
    },
    models::{generated::chat_capnp, pending_operation::OpType},
    storage::store_trait::impl_crdt_model,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Reconcile, Hydrate)]
pub enum ContactStatus {
    Pending,
    Active,
}

capnp_enum_convert!(ContactStatus => chat_capnp::ContactStatus {
    Pending,
    Active,
});

/// A chat contact entry.
#[derive(Debug, Clone, PartialEq, Eq, Reconcile, Hydrate)]
pub struct ChatContact {
    #[autosurgeon(with = "crate::chat::models::arc_str")]
    pub username: Arc<str>,
    #[autosurgeon(with = "crate::chat::models::arc_str")]
    pub dht_key: Arc<str>,
    #[autosurgeon(with = "crate::chat::models::arc_str")]
    pub display_name: Arc<str>,
    #[autosurgeon(with = "crate::chat::models::arc_str")]
    pub conversation_record_key: Arc<str>,
    pub status: ContactStatus,
}

impl ChatContact {
    pub fn new(username: &str, dht_key: &str, display_name: &str) -> Self {
        Self {
            username: Arc::from(username),
            dht_key: Arc::from(dht_key),
            display_name: Arc::from(display_name),
            conversation_record_key: Arc::from(""),
            status: ContactStatus::Pending,
        }
    }
}

/// List of chat contacts, keyed by DHT key.
#[derive(Debug, Clone, Default, Reconcile, Hydrate)]
pub struct ContactList {
    pub contacts: HashMap<String, ChatContact>,
    pub last_updated: u64,
}

impl ContactList {
    pub fn add_contact(&mut self, contact: ChatContact) {
        if !self.contacts.contains_key(contact.dht_key.as_ref()) {
            self.last_updated = now_epoch_millis();
            self.contacts.insert(contact.dht_key.to_string(), contact);
        }
    }

    pub fn find_by_dht_key(&self, dht_key: &str) -> Option<&ChatContact> {
        self.contacts.get(dht_key)
    }

    pub fn remove_by_dht_key(&mut self, dht_key: &str) -> bool {
        if self.contacts.remove(dht_key).is_some() {
            self.last_updated = now_epoch_millis();
            true
        } else {
            false
        }
    }
}

const CONTACT_SCHEMA_VERSION: u32 = 3;

fn pack_contact_list(list: &ContactList) -> Result<Vec<u8>> {
    pack::<chat_capnp::ChatContactList>(|mut root| {
        root.schema_version().set(CONTACT_SCHEMA_VERSION);
        root.last_updated().set(list.last_updated);
        let len = capnp_u32(list.contacts.len());
        let mut contacts = root.contacts().init(len);
        for (i, contact) in list.contacts.values().enumerate() {
            let idx = capnp_u32(i);
            let mut entry = contacts.at(idx).get();
            entry.username().set_str(&contact.username);
            entry.dht_key().set_str(&contact.dht_key);
            entry.display_name().set_str(&contact.display_name);
            entry
                .conversation_record_key()
                .set_str(&contact.conversation_record_key);
            entry.status().set(contact.status.into());
        }
    })
}

fn unpack_contact_list(bytes: &[u8]) -> Result<(ContactList, u32)> {
    unpack::<chat_capnp::ChatContactList, _>(bytes, |root| {
        let contacts_capnp = root.contacts().get();
        let mut contacts = HashMap::with_capacity(contacts_capnp.len() as usize);
        for i in 0..contacts_capnp.len() {
            let c = contacts_capnp.at(i);
            let contact = ChatContact {
                username: Arc::from(c.username().get().as_str()?),
                dht_key: Arc::from(c.dht_key().get().as_str()?),
                display_name: Arc::from(c.display_name().get().as_str()?),
                conversation_record_key: Arc::from(c.conversation_record_key().get().as_str()?),
                status: c.status()?.into(),
            };
            contacts.insert(contact.dht_key.to_string(), contact);
        }
        let version = root.schema_version();
        Ok((
            ContactList {
                contacts,
                last_updated: root.last_updated(),
            },
            version,
        ))
    })
}

impl_crdt_model!(
    ContactList,
    id = "contacts",
    display = "ContactList",
    op = OpType::ChatContacts,
    schema = CONTACT_SCHEMA_VERSION,
    pack = pack_contact_list,
    unpack = unpack_contact_list,
);
