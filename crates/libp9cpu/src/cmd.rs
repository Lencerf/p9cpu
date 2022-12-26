

use thiserror::Error;

tonic::include_proto!("cmd");

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

impl Command {
    pub fn new(program: String) -> Command {
        Command {
            program,
            ..Default::default()
        }
    }

    pub fn args(&mut self, args: impl IntoIterator<Item = String>) -> &mut Command {
        self.args.extend(args);
        self
    }

    pub fn env(&mut self, key: impl Into<Vec<u8>>, val: impl Into<Vec<u8>>) -> &mut Command {
        self.envs.push(EnvVar {
            key: key.into(),
            val: val.into(),
        });
        self
    }

    pub fn fstab(&mut self, tab: FsTab) -> &mut Command {
        self.fstab.push(tab);
        self
    }
}
