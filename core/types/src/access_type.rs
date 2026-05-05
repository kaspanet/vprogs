#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AccessType {
    Read = 0,
    Write = 1,
}

impl From<AccessType> for u8 {
    fn from(s: AccessType) -> Self {
        s as u8
    }
}

impl TryFrom<u8> for AccessType {
    type Error = ();
    fn try_from(v: u8) -> Result<Self, Self::Error> {
        match v {
            0 => Ok(AccessType::Read),
            1 => Ok(AccessType::Write),
            _ => Err(()),
        }
    }
}
