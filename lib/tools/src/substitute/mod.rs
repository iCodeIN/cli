mod spec;

mod util;

use atty;
use yaml_rust;
use failure::{err_msg, Error, ResultExt};
use handlebars::{no_escape, Handlebars};

use std::ffi::OsStr;
use std::io::{self, stdin};
use std::fs::File;
use std::os::unix::ffi::OsStrExt;

pub use self::spec::*;
use self::util::{de_json_or_yaml, validate};
use std::collections::BTreeSet;

pub fn substitute(
    input_data: &StreamOrPath,
    specs: &[Spec],
    separator: &OsStr,
    try_deserialize: bool,
) -> Result<(), Error> {
    use self::StreamOrPath::*;
    let mut own_specs = Vec::new();

    let (dataset, specs) = match *input_data {
        Stream => if atty::is(atty::Stream::Stdin) {
            bail!("Stdin is a TTY. Cannot substitute a template without any data.")
        } else {
            let stdin = stdin();
            let locked_stdin = stdin.lock();
            (de_json_or_yaml(locked_stdin)?, specs)
        },
        Path(ref p) => (
            de_json_or_yaml(File::open(&p).context(format!("Could not open input data file at '{}'", p.display()))?)?,
            if specs.is_empty() {
                own_specs.push(Spec {
                    src: Stream,
                    dst: Stream,
                });
                &own_specs
            } else {
                specs
            },
        ),
    };

    validate(input_data, specs)?;

    let mut seen_file_outputs = BTreeSet::new();
    let mut seen_writes_to_stdout = 0;
    let mut hbs = Handlebars::new();
    hbs.set_strict_mode(true);
    hbs.register_escape_fn(no_escape);
    let mut buf = Vec::<u8>::new();

    for spec in specs {
        let append = match spec.dst {
            Path(ref p) => {
                let seen = seen_file_outputs.contains(p);
                seen_file_outputs.insert(p);
                seen
            }
            Stream => {
                seen_writes_to_stdout += 1;
                false
            }
        };

        let mut istream = spec.src.open_as_input()?;
        hbs.register_template_source(spec.src.short_name(), &mut istream)
            .with_context(|_| format!("Failed to register handlebars template at '{}'", spec.src.name()))?;

        let mut ostream = spec.dst.open_as_output(append)?;
        if seen_writes_to_stdout > 1 || append {
            ostream.write_all(separator.as_bytes())?;
        }

        {
            let ostream_for_template: &mut io::Write = if try_deserialize { &mut buf } else { &mut ostream };

            hbs.render_to_write(spec.src.short_name(), &dataset, ostream_for_template)
                .with_context(|_| format!("Could instantiate template or writing to '{}' failed", spec.dst.name()))?;
        }

        if try_deserialize {
            {
                let str_buf = ::std::str::from_utf8(&buf).context(format!(
                    "Validation of template output at '{}' failed as it was not valid UTF8",
                    spec.dst.name()
                ))?;
                yaml_rust::YamlLoader::load_from_str(str_buf).context(format!(
                    "Validation of template output at '{}' failed. It's neither valid YAML, nor JSON",
                    spec.dst.name()
                ))?;
            }
            let mut read = io::Cursor::new(buf);
            io::copy(&mut read, &mut ostream)
                .map_err(|_| err_msg("Failed to output validated template to destination."))?;
            buf = read.into_inner();
            buf.clear();
        }
    }
    Ok(())
}
