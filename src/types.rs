use solana_sdk::{
    clock::Slot,
    signature::Signature,
    transaction::VersionedTransaction,
};

pub struct TransactionEvent<'a> {
    pub slot: Slot,
    pub signature: Signature,
    pub transaction: &'a VersionedTransaction,
    pub processed_at_micros: u64,
}

pub trait ShredHandler: Send + Sync {
    fn handle_transaction(&self, event: &TransactionEvent<'_>);
}

#[derive(thiserror::Error, Debug)]
pub enum DecoderError {
    #[error("failed to bind UDP socket on {addr}: {source}")]
    Bind {
        addr: String,
        #[source]
        source: std::io::Error,
    },
    #[error("handler not provided — call builder.handler(...) before build()")]
    MissingHandler,
    #[error("bind address not provided — call builder.bind(...) before build()")]
    MissingBind,
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("other: {0}")]
    Other(String),
}
