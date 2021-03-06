use conv::TryFrom;
use std::fmt;
use std::fs::create_dir_all;
use std::fs::{File, OpenOptions};
use std::io::{self, stdin, Read, Write};

use crate::util::run_editor;
use atty;
use failure::{Error, ResultExt};
use mktemp::Temp;
use std::env;
use std::ffi::OsString;
use std::path::Component;
use std::path::{Path, PathBuf};

lazy_static! {
    static ref EDITOR: PathBuf = PathBuf::from(env::var_os("EDITOR").unwrap_or_else(|| OsString::from("vim")));
}

#[derive(Debug, Clone, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub enum SpecSourceType {
    Stdin,
    Path(PathBuf),
}

impl fmt::Display for SpecSourceType {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match *self {
            SpecSourceType::Stdin => {
                let empty = PathBuf::new();
                empty.display().fmt(f)
            }
            SpecSourceType::Path(ref p) => p.display().fmt(f),
        }
    }
}

#[derive(Debug, Clone, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub struct VaultSpec {
    pub src: SpecSourceType,
    pub dst: PathBuf,
}

#[derive(Debug, Clone, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub struct VaultSpecError(pub String);

impl fmt::Display for VaultSpecError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl fmt::Display for VaultSpec {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{}:{}", self.src, self.dst.display())
    }
}

impl ::std::error::Error for VaultSpecError {
    fn description(&self) -> &str {
        "The vault spec was invalid"
    }
}

pub fn gpg_output_filename(path: &Path) -> Result<PathBuf, Error> {
    let file_name = path
        .file_name()
        .ok_or_else(|| format_err!("'{}' does not have a filename", path.display()))?;
    Ok(path.parent().expect("path with filename to have a root").join(format!(
        "{}.gpg",
        file_name.to_str().expect("filename to be decodeable with UTF8",)
    )))
}

struct TemporaryFile {
    _tempfile: Temp,
    open_file: File,
}

impl Read for TemporaryFile {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.open_file.read(buf)
    }
}

impl VaultSpec {
    pub fn source(&self) -> Option<&Path> {
        match self.src {
            SpecSourceType::Stdin => None,
            SpecSourceType::Path(ref p) => Some(p.as_path()),
        }
    }

    pub fn destination(&self) -> &Path {
        &self.dst
    }

    pub fn output_in(&self, root: &Path, dst_mode: Destination) -> Result<PathBuf, Error> {
        Ok(match dst_mode {
            Destination::ReolveAndAppendGpg => root.join(gpg_output_filename(&self.dst)?),
            Destination::Unchanged => self.dst.to_owned(),
        })
    }

    pub fn open_output_in(
        &self,
        root: &Path,
        mode: WriteMode,
        dst_mode: Destination,
        output: &mut dyn Write,
    ) -> Result<File, Error> {
        let output_file = self.output_in(root, dst_mode)?;
        if let Some(d) = output_file.parent() {
            if !d.is_dir() {
                create_dir_all(d)
                    .with_context(|_| format!("Failed to created intermediate directory at '{}'", d.display()))?;
                writeln!(output, "Created intermediate directory at '{}'", d.display()).ok();
            }
        }
        if mode.refuse_overwrite() && output_file.exists() {
            return Err(format_err!(
                "Refusing to overwrite existing file at '{}'",
                output_file.display()
            ));
        }
        Ok(OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(&output_file)
            .with_context(|_| {
                format!(
                    "Could not open destination file at '{}' for writing.",
                    output_file.display()
                )
            })?)
    }

    pub fn open_input(&self) -> Result<Box<dyn Read>, Error> {
        Ok(match self.src {
            SpecSourceType::Path(ref p) => {
                Box::new(File::open(p).with_context(|_| format!("Could not open input file at '{}'", p.display()))?)
            }
            SpecSourceType::Stdin => {
                if atty::is(atty::Stream::Stdin) {
                    let tempfile = Temp::new_file().with_context(|_| "Failed to obtain temporary file for editing.")?;
                    let tempfile_path = tempfile.to_path_buf();
                    run_editor(EDITOR.as_os_str(), &tempfile_path)?;
                    Box::new(TemporaryFile {
                        _tempfile: tempfile,
                        open_file: File::open(&tempfile_path).with_context(|_| {
                            format!(
                                "Could not open temporary file '{}' for reading.",
                                tempfile_path.display()
                            )
                        })?,
                    })
                } else {
                    Box::new(stdin())
                }
            }
        })
    }
}

impl<'a> TryFrom<&'a str> for VaultSpec {
    type Err = VaultSpecError;

    fn try_from(input: &'a str) -> Result<Self, Self::Err> {
        const SEPARATOR: char = ':';
        fn has_parent_component(p: &Path) -> bool {
            p.components().any(|c| match c {
                Component::ParentDir => true,
                _ => false,
            })
        }
        let validate = |src: &'a str| {
            Ok(if src.is_empty() {
                SpecSourceType::Stdin
            } else {
                SpecSourceType::Path(PathBuf::from(src))
            })
        };
        let validate_dst = |p: PathBuf| {
            if p.is_absolute() {
                Err(VaultSpecError(format!(
                    "'{}' must not have an absolute destination.",
                    input
                )))
            } else {
                Ok(p)
            }
        };

        if input.is_empty() {
            return Err(VaultSpecError("An empty spec is invalid.".into()));
        }
        let mut splits = input.splitn(2, SEPARATOR);
        Ok(match (splits.next(), splits.next()) {
            (Some(src), None) => VaultSpec {
                src: validate(src)?,
                dst: {
                    let dst = validate_dst(PathBuf::from(src)).map_err(|mut e| {
                        e.0.push_str(" Try specifying the destination explicitly.");
                        e
                    })?;
                    if has_parent_component(&dst) {
                        return Err(VaultSpecError(format!(
                            "Relative parent directories in source '{}' need the destination set explicitly.",
                            src
                        )));
                    };
                    dst
                },
            },
            (Some(src), Some(dst)) => VaultSpec {
                src: validate(src)?,
                dst: PathBuf::from(if dst.is_empty() {
                    if src.is_empty() {
                        return Err(VaultSpecError(format!("'{}' does not contain a destination.", input)));
                    }
                    src
                } else if dst.contains(SEPARATOR) {
                    return Err(VaultSpecError(format!(
                        "'{}' must not contain more than one colon.",
                        input
                    )));
                } else {
                    dst
                }),
            },
            _ => unreachable!(),
        })
    }
}

#[cfg(test)]
mod tests_gpg_output_filename {
    use super::gpg_output_filename;
    use std::path::Path;
    use std::path::PathBuf;

    #[test]
    fn it_appends_the_gpg_suffix_to_file_names() {
        assert_eq!(
            gpg_output_filename(Path::new("a/file")).unwrap(),
            PathBuf::from("a/file.gpg")
        )
    }

    #[test]
    fn it_appends_the_gpg_suffix_to_file_names_with_extension() {
        assert_eq!(
            gpg_output_filename(Path::new("a/file.ext")).unwrap(),
            PathBuf::from("a/file.ext.gpg")
        )
    }
}

#[derive(Debug, Clone, Copy, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub enum Destination {
    ReolveAndAppendGpg,
    Unchanged,
}

#[derive(Debug, Clone, Copy, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub enum WriteMode {
    AllowOverwrite,
    RefuseOverwrite,
}

#[derive(Debug, Clone, Copy, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub enum SigningMode {
    None,
    Public,
}

#[derive(Debug, Clone, Copy, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub enum CreateMode {
    Create,
    NoCreate,
}

impl WriteMode {
    pub fn refuse_overwrite(self) -> bool {
        match self {
            WriteMode::AllowOverwrite => false,
            WriteMode::RefuseOverwrite => true,
        }
    }
}
