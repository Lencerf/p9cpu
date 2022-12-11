pub type FsTab = crate::rpc::FsTab;
use thiserror::Error;

#[derive(Error, Debug)]
pub enum FsTabError {
    #[error("line does not contain 6 fields.")]
    NotSixFields,
    #[error("Invalid Number for {0}: {1}")]
    InvalidNumber(&'static str, std::num::ParseIntError),
}

impl TryFrom<&str> for FsTab {
    type Error = FsTabError;
    fn try_from(line: &str) -> Result<Self, Self::Error> {
        let mut it = line.split_whitespace();
        let spec = it.next().ok_or(FsTabError::NotSixFields)?.to_string();
        let file = it.next().ok_or(FsTabError::NotSixFields)?.to_string();
        let vfstype = it.next().ok_or(FsTabError::NotSixFields)?.to_string();
        let mntops = it.next().ok_or(FsTabError::NotSixFields)?.to_string();
        let freq = it
            .next()
            .ok_or(FsTabError::NotSixFields)?
            .parse()
            .map_err(|e| FsTabError::InvalidNumber("freq", e))?;
        let passno = it
            .next()
            .ok_or(FsTabError::NotSixFields)?
            .parse()
            .map_err(|e| FsTabError::InvalidNumber("passno", e))?;
        if it.next().is_some() {
            return Err(FsTabError::NotSixFields);
        }
        Ok(FsTab {
            spec,
            file,
            vfstype,
            mntops,
            freq,
            passno,
        })
    }
}
