use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::fs::{remove_file, File};
use std::mem;

use mktemp::Temp;
use itertools::join;
use gpgme;
use base::Vault;
use failure::{Error, ResultExt};
use error::FailExt;
use sheesy_types::{gpg_output_filename, CreateMode, Destination, SpecSourceType, VaultSpec, WriteMode};
use error::{DecryptionError, EncryptionError};
use util::{new_context, strip_ext, write_at};
use sheesy_types::run_editor;
use std::iter::once;

fn encrypt_buffer(ctx: &mut gpgme::Context, input: &[u8], keys: &[gpgme::Key]) -> Result<Vec<u8>, Error> {
    let mut encrypted_bytes = Vec::<u8>::new();
    ctx.encrypt(keys, input, &mut encrypted_bytes)
        .map_err(|e: gpgme::Error| EncryptionError::caused_by(e, "Failed to encrypt data.".into(), ctx, keys))?;
    Ok(encrypted_bytes)
}

impl Vault {
    pub fn edit(
        &self,
        path: &Path,
        editor: &Path,
        mode: &CreateMode,
        try_encrypt: bool,
        output: &mut Write,
    ) -> Result<(), Error> {
        let file = Temp::new_file().context("Could not create temporary file to decrypt to.")?;
        let tempfile_path = file.to_path_buf();
        let decrypted_file_path = {
            let mut decrypted_writer =
                write_at(&tempfile_path).context("Failed to open temporary file for writing decrypted content to.")?;
            self.decrypt(path, &mut decrypted_writer)
                .context(format!("Failed to decrypt file at '{}'.", path.display()))
                .or_else(|err| match (mode, err.first_cause_of::<io::Error>()) {
                    (&CreateMode::Create, Some(_)) => gpg_output_filename(path).map(|p| self.absolute_path(&p)),
                    _ => Err(err.into()),
                })?
        };
        if try_encrypt {
            self.encrypt_buffer(b"")
                .context("Aborted edit operation as you cannot encrypt resources.")?;
        }
        run_editor(editor.as_os_str(), &tempfile_path)?;
        let mut zero = Vec::new();
        self.encrypt(
            &[
                VaultSpec {
                    src: SpecSourceType::Path(tempfile_path),
                    dst: decrypted_file_path,
                },
            ],
            WriteMode::AllowOverwrite,
            Destination::Unchanged,
            &mut zero,
        ).context("Failed to re-encrypt edited content.")?;
        writeln!(output, "Edited '{}'.", path.display()).ok();
        Ok(())
    }

    pub fn decrypt(&self, path: &Path, w: &mut Write) -> Result<PathBuf, Error> {
        let mut ctx = new_context()?;
        let (partition, spec) = self.partition_by_owned_spec(VaultSpec {
            src: SpecSourceType::Stdin,
            dst: path.to_owned(),
        })?;
        let resolved_absolute_path = partition.secrets_path().join(spec.destination());
        let resolved_gpg_path = gpg_output_filename(&resolved_absolute_path)?;
        let (mut input, path_for_decryption) = File::open(&resolved_gpg_path)
            .map(|f| (f, resolved_gpg_path.to_owned()))
            .or_else(|_| File::open(&resolved_absolute_path).map(|f| (f, resolved_absolute_path.to_owned())))
            .context(format!(
                "Could not open input file at '{}' for reading. Tried '{}' as well.",
                resolved_gpg_path.display(),
                resolved_absolute_path.display()
            ))?;
        let mut output = Vec::new();
        ctx.decrypt(&mut input, &mut output)
            .map_err(|e: gpgme::Error| DecryptionError::caused_by(e, "Failed to decrypt data."))?;

        w.write_all(&output)
            .context("Could not write out all decrypted data.")?;
        Ok(path_for_decryption)
    }

    pub fn remove(&self, specs: &[PathBuf], output: &mut Write) -> Result<(), Error> {
        for path_to_remove in specs {
            let path = {
                let spec = VaultSpec {
                    src: SpecSourceType::Stdin,
                    dst: path_to_remove.clone(),
                };
                let (partition, spec) = self.partition_by_owned_spec(spec)?;
                let gpg_path = spec.output_in(&partition.secrets_path(), Destination::ReolveAndAppendGpg)?;
                if gpg_path.exists() {
                    gpg_path
                } else {
                    let mut new_path = strip_ext(&gpg_path);
                    if !new_path.exists() {
                        return Err(format_err!("No file present at '{}'", gpg_path.display()));
                    }
                    new_path
                }
            };
            remove_file(&path).context(format!("Failed to remove file at '{}'.", path.display()))?;
            writeln!(output, "Removed file at '{}'", path.display()).ok();
        }
        Ok(())
    }

    pub fn encrypt_buffer(&self, input: &[u8]) -> Result<Vec<u8>, Error> {
        let mut ctx = new_context()?;
        let keys = self.recipient_keys(&mut ctx)?;

        let encrypted_bytes = encrypt_buffer(&mut ctx, input, &keys)?;
        Ok(encrypted_bytes)
    }

    pub fn partition_by_owned_spec(&self, spec: VaultSpec) -> Result<(&Vault, VaultSpec), Error> {
        if self.partitions.is_empty() {
            Ok((self, spec))
        } else {
            let partition = once(self)
                .chain(&self.partitions)
                .find(|p| spec.dst.starts_with(&p.secrets))
                .ok_or_else(|| {
                    format_err!("Spec '{}' could not be associated with any partition. Prefix it with the partition resource directory.", spec)
                })?;
            Ok((
                partition,
                VaultSpec {
                    src: spec.src,
                    dst: spec.dst
                        .strip_prefix(&partition.secrets)
                        .expect("success if 'starts_with' succeeds")
                        .to_owned(),
                },
            ))
        }
    }
    pub fn partition_by_spec(&self, spec: &VaultSpec) -> Result<(&Vault, VaultSpec), Error> {
        self.partition_by_owned_spec(spec.clone())
    }
    pub fn encrypt(
        &self,
        specs: &[VaultSpec],
        mode: WriteMode,
        dst_mode: Destination,
        output: &mut Write,
    ) -> Result<(), Error> {
        let mut ctx = new_context()?;
        let mut lut: Vec<Option<(PathBuf, Vec<gpgme::Key>)>> = vec![None; 1 + self.partitions.len()];
        let mut encrypted_destinations = Vec::new();

        for spec in specs {
            {
                let (partition, spec) = self.partition_by_spec(spec)?;
                let (secrets_dir, keys) = match &mut lut[partition.index] {
                    &mut Some((ref secrets_dir, ref keys)) => (secrets_dir, keys),
                    none => {
                        mem::replace(
                            none,
                            Some((
                                partition.secrets_path(),
                                partition.recipient_keys(&mut ctx)?,
                            )),
                        );
                        let some = none;
                        let &(ref secrets_dir, ref keys) = some.as_ref().expect("the content that was just put in");
                        (secrets_dir, keys)
                    }
                };
                let input = {
                    let mut buf = Vec::new();
                    spec.open_input()?.read_to_end(&mut buf).context(format!(
                        "Could not read all input from '{}' into buffer.",
                        spec.source()
                            .map(|s| format!("{}", s.display()))
                            .unwrap_or_else(|| "<stdin>".into())
                    ))?;
                    buf
                };
                let mut encrypted_bytes = encrypt_buffer(&mut ctx, &input, keys)?;
                spec.open_output_in(secrets_dir, mode, dst_mode, output)?
                    .write_all(&encrypted_bytes)
                    .context(format!(
                        "Failed to write all encrypted data to '{}'.",
                        spec.destination().display(),
                    ))?;
            }
            encrypted_destinations.push(spec.destination());
        }
        writeln!(
            output,
            "Added {}.",
            join(
                encrypted_destinations
                    .iter()
                    .map(|p| format!("'{}'", p.display())),
                ", "
            )
        ).ok();
        Ok(())
    }
}
