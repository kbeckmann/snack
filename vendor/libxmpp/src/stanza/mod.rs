pub mod stream;
pub mod sasl;
pub mod bind;
pub mod muc;
pub mod chat;

pub trait Stanza
{
    fn to_xml(&self) -> String;

    fn as_bytes(&self) -> Vec<u8>
    {
        return self.to_xml().into_bytes();
    }
}
