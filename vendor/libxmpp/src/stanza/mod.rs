pub mod stream;
pub mod sasl;
pub mod bind;
pub mod muc;
pub mod chat;
pub mod ping;
pub mod disco;
pub mod sm;
pub mod caps;
pub mod delay;

pub trait Stanza
{
    fn to_xml(&self) -> String;

    fn as_bytes(&self) -> Vec<u8>
    {
        return self.to_xml().into_bytes();
    }
}
