//! Cap'n Proto pack/unpack helpers and enum-conversion macro using `recapn`
//! with packed encoding. `src/bin/sample-generator.rs` includes this file
//! via `#[path]` so samples train against runtime encoding.

use std::io::Cursor;

use anyhow::Result;
use recapn::{
    io::{self, StreamOptions},
    message::{Message, Reader as MessageReader, ReaderOptions},
    rpc::Empty,
};

/// Pack a Cap'n Proto struct into bytes.
///
/// `build` receives a mutable root builder for the schema type `T`.
pub fn pack<T: recapn::ty::Struct>(build: impl FnOnce(T::Builder<'_, Empty>)) -> Result<Vec<u8>> {
    let mut message = Message::global();
    let mut builder = message.builder();
    let root = builder.by_ref().init_struct_root::<T>();
    build(root);
    let segments = builder.segments();
    // Pre-allocate based on total segment size. Packed encoding is typically
    // smaller than raw segments, so this avoids 3-5 re-allocations.
    let capacity: usize = segments.into_iter().map(|seg| seg.as_bytes().len()).sum();
    let mut buf = Vec::with_capacity(capacity);
    io::write_message_packed(&mut buf, &builder.segments())?;
    Ok(buf)
}

/// Unpack bytes into a value by reading a Cap'n Proto struct.
///
/// `read` receives the root reader for the schema type `T` and returns the result.
pub fn unpack<T: recapn::ty::Struct, R>(
    bytes: &[u8],
    read: impl FnOnce(T::Reader<'_, Empty>) -> Result<R>,
) -> Result<R> {
    let message = io::read_from_packed_stream(Cursor::new(bytes), StreamOptions::default())?;
    let reader = MessageReader::new(&message, ReaderOptions::default());
    let root = reader.read_as_struct::<T>();
    read(root)
}

/// Converts a `usize` to `u32` without truncation, clamping at `u32::MAX`.
///
/// Named for its original use (Cap'n Proto list lengths and indices), but it
/// is a general saturating narrowing — `storage/backup.rs` (worker count) and
/// `compression/trainer.rs` (f64-exact size math) use it for the same reason.
/// This app's values are far below `u32::MAX`; overflow clamps rather than
/// wrapping.
#[must_use]
pub fn capnp_u32(value: usize) -> u32 {
    u32::try_from(value).unwrap_or(u32::MAX)
}

/// Generates bidirectional `From` impls between a Rust enum and its Cap'n Proto
/// counterpart. Both enums must have identically-named variants.
///
/// # Example
///
/// ```ignore
/// capnp_enum_convert!(ContactStatus => chat_capnp::ContactStatus {
///     Pending,
///     Active,
/// });
/// ```
macro_rules! capnp_enum_convert {
    ($rust:ty => $capnp:ty { $($variant:ident),+ $(,)? }) => {
        impl From<$capnp> for $rust {
            fn from(v: $capnp) -> Self {
                match v {
                    $( <$capnp>::$variant => Self::$variant, )+
                }
            }
        }

        impl From<$rust> for $capnp {
            fn from(v: $rust) -> Self {
                match v {
                    $( <$rust>::$variant => Self::$variant, )+
                }
            }
        }
    };
}

pub(crate) use capnp_enum_convert;
