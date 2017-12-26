use types::{Vault, VaultExt};
use s3_types::VaultContext;
use failure::Error;
use init::init;
use resource;

/// A universal handler which delegates all functionality based on the provided Context
/// The latter is usually provided by the user interface.
pub fn do_it(ctx: VaultContext) -> Result<String, Error> {
    use s3_types::VaultCommand;
    match ctx.command {
        VaultCommand::Init {
            gpg_key_ids,
            gpg_keys_dir,
            recipients_file,
        } => init(
            &gpg_key_ids,
            &gpg_keys_dir,
            &recipients_file,
            &ctx.vault_path,
            {
                let r: Result<usize, _> = ctx.vault_id.parse();
                match r {
                    Err(_) => Some(ctx.vault_id),
                    Ok(_) => None,
                }
            },
        ),
        VaultCommand::ResourceAdd { specs } => resource::add(
            Vault::from_file(&ctx.vault_path)?.select(&ctx.vault_id)?,
            &specs,
        ),
        VaultCommand::List => {
            Vault::from_file(&ctx.vault_path)?;
            Ok(String::new())
        }
    }
}
