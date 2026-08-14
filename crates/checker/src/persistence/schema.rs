//! MDBX tables and fixed-width key encodings for persisted checker records.

use super::{
    CheckpointChunk, CheckpointChunkKey, CheckpointId, CheckpointManifest, Finding, FindingKey,
    JournalEntry, MetaValue,
};
use reth_db::{
    DatabaseError, TableSet,
    table::{Decode, Encode, Table, TableInfo},
};
use serde::{Deserialize, Serialize};

/// Key for one singleton metadata value.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub(crate) enum MetaKey {
    Version,
    Metadata,
}
impl Encode for MetaKey {
    type Encoded = [u8; 1];
    fn encode(self) -> Self::Encoded {
        [match self {
            Self::Version => 0,
            Self::Metadata => 1,
        }]
    }
}
impl Decode for MetaKey {
    fn decode(v: &[u8]) -> Result<Self, DatabaseError> {
        match v {
            [0] => Ok(Self::Version),
            [1] => Ok(Self::Metadata),
            _ => Err(DatabaseError::Decode),
        }
    }
}

/// Implement fixed-width database key encoding for one durable identifier.
macro_rules! fixed_key {
    ($t:ty,$n:expr,$enc:expr,$dec:expr) => {
        impl Encode for $t {
            type Encoded = [u8; $n];
            fn encode(self) -> Self::Encoded {
                $enc(self)
            }
        }
        impl Decode for $t {
            fn decode(v: &[u8]) -> Result<Self, DatabaseError> {
                if v.len() != $n {
                    return Err(DatabaseError::Decode);
                };
                $dec(v)
            }
        }
    };
}
fixed_key!(
    CheckpointId,
    40,
    |v: CheckpointId| {
        let mut b = [0; 40];
        b[..8].copy_from_slice(&v.height.to_be_bytes());
        b[8..].copy_from_slice(v.hash.as_slice());
        b
    },
    |v: &[u8]| Ok(CheckpointId {
        height: u64::from_be_bytes(v[..8].try_into().map_err(|_| DatabaseError::Decode)?),
        hash: alloy_primitives::B256::from_slice(&v[8..])
    })
);
fixed_key!(
    CheckpointChunkKey,
    44,
    |v: CheckpointChunkKey| {
        let mut b = [0; 44];
        b[..8].copy_from_slice(&v.checkpoint.height.to_be_bytes());
        b[8..40].copy_from_slice(v.checkpoint.hash.as_slice());
        b[40..].copy_from_slice(&v.index.to_be_bytes());
        b
    },
    |v: &[u8]| Ok(CheckpointChunkKey {
        checkpoint: CheckpointId {
            height: u64::from_be_bytes(v[..8].try_into().map_err(|_| DatabaseError::Decode)?),
            hash: alloy_primitives::B256::from_slice(&v[8..40]),
        },
        index: u32::from_be_bytes(v[40..].try_into().map_err(|_| DatabaseError::Decode)?),
    })
);
fixed_key!(
    FindingKey,
    46,
    |v: FindingKey| {
        let mut b = [0; 46];
        b[..8].copy_from_slice(&v.zone.number.to_be_bytes());
        b[8..40].copy_from_slice(v.zone.hash.as_slice());
        b[40..44].copy_from_slice(&v.operation.to_be_bytes());
        b[44..].copy_from_slice(&v.code.to_be_bytes());
        b
    },
    |v: &[u8]| Ok(FindingKey {
        zone: super::BlockNumHash {
            number: u64::from_be_bytes(v[..8].try_into().map_err(|_| DatabaseError::Decode)?),
            hash: alloy_primitives::B256::from_slice(&v[8..40])
        },
        operation: u32::from_be_bytes(v[40..44].try_into().map_err(|_| DatabaseError::Decode)?),
        code: u16::from_be_bytes(v[44..].try_into().map_err(|_| DatabaseError::Decode)?)
    })
);

/// Declare one checker persistence table and its static metadata.
macro_rules! table {
    ($name:ident,$key:ty,$value:ty,$db:literal) => {
        #[derive(Debug)]
        pub(crate) struct $name;
        impl Table for $name {
            const NAME: &'static str = $db;
            const DUPSORT: bool = false;
            type Key = $key;
            type Value = $value;
        }
        impl TableInfo for $name {
            fn name(&self) -> &'static str {
                $db
            }
            fn is_dupsort(&self) -> bool {
                false
            }
        }
    };
}
table!(Meta, MetaKey, MetaValue, "Meta");
table!(Checkpoints, CheckpointId, CheckpointManifest, "Checkpoints");
table!(
    CheckpointChunks,
    CheckpointChunkKey,
    CheckpointChunk,
    "CheckpointChunks"
);
table!(Journal, u64, JournalEntry, "Journal");
table!(Findings, FindingKey, Finding, "Findings");

/// Complete table set for one checker persistence database.
pub(crate) struct PersistenceTables;
impl TableSet for PersistenceTables {
    fn tables() -> Box<dyn Iterator<Item = Box<dyn TableInfo>>> {
        Box::new(
            vec![
                Box::new(Meta) as Box<dyn TableInfo>,
                Box::new(Checkpoints),
                Box::new(CheckpointChunks),
                Box::new(Journal),
                Box::new(Findings),
            ]
            .into_iter(),
        )
    }
}
