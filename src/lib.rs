//! High-level UDP shred decoder with parallel Reed-Solomon recovery.
//!
//! ```ignore
//! use shred_decoder::{ShredDecoder, ShredHandler, TransactionEvent};
//!
//! struct MyHandler;
//! impl ShredHandler for MyHandler {
//!     fn handle_transaction(&self, event: &TransactionEvent<'_>) {
//!         println!("slot={} sig={}", event.slot, event.signature);
//!     }
//! }
//!
//! let decoder = ShredDecoder::builder()
//!     .bind("0.0.0.0:20000".parse().unwrap())
//!     .recovery_workers(8)
//!     .handler(MyHandler)
//!     .build()
//!     .unwrap();
//!
//! // … later …
//! decoder.shutdown();
//! ```

mod decoder;
mod recovery;
mod state;
mod types;

pub use decoder::{DecoderStats, ShredDecoder, ShredDecoderBuilder};
pub use types::{DecoderError, ShredHandler, TransactionEvent};

// Re-exports so downstream users don't need to pin solana 2.2.1 themselves.
pub use solana_sdk::{
    clock::Slot,
    signature::Signature,
    transaction::VersionedTransaction,
};
