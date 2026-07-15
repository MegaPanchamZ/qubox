#[cfg(feature = "wire-rkyv-v2")]
pub type CheckTypeError = rkyv::rancor::Error;

#[cfg(feature = "wire-rkyv-v2")]
pub type RkyvBytes = rkyv::util::AlignedVec;

#[cfg(feature = "wire-rkyv-v2")]
pub const WIRE_FORMAT_VERSION_RKYV_V2: u8 = 0x02;

#[cfg(feature = "wire-rkyv-v2")]
pub const MEDIA_DATAGRAM_MAGIC_V2: [u8; 2] = [0x52, 0x42];

#[cfg(feature = "wire-rkyv-v2")]
pub fn to_rkyv_bytes<T>(value: &T) -> Result<RkyvBytes, CheckTypeError>
where
    T: for<'a> rkyv::Serialize<
        rkyv::api::high::HighSerializer<
            rkyv::util::AlignedVec,
            rkyv::ser::allocator::ArenaHandle<'a>,
            CheckTypeError,
        >,
    >,
{
    rkyv::to_bytes::<CheckTypeError>(value)
}

#[cfg(feature = "wire-rkyv-v2")]
pub fn wire_error(message: &'static str) -> CheckTypeError {
    use rkyv::rancor::Source;
    CheckTypeError::new(std::io::Error::new(
        std::io::ErrorKind::InvalidData,
        message,
    ))
}
