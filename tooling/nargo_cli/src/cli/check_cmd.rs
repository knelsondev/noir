use crate::errors::CliError;

use clap::Args;
use fm::FileManager;
use iter_extended::btree_map;
use nargo::{
    errors::CompileError, insert_all_files_for_workspace_into_file_manager,
    ops::check_crate_and_report_errors, package::Package, parse_all, prepare_package,
    workspace::Workspace,
};
use nargo_toml::PackageSelection;
use noir_artifact_cli::fs::artifact::write_to_file;
use noirc_abi::{AbiParameter, AbiType, MAIN_RETURN_NAME};
use noirc_driver::{CompileOptions, check_crate, compute_function_abi};
use noirc_frontend::{hir::ParsedFiles, monomorphization::monomorphize};

use super::{LockType, PackageOptions, WorkspaceCommand};

/// Check a local package and all of its dependencies for errors
#[derive(Debug, Clone, Args)]
#[clap(visible_alias = "c")]
pub(crate) struct CheckCommand {
    #[clap(flatten)]
    pub(super) package_options: PackageOptions,

    /// Force overwrite of existing files
    #[clap(long = "overwrite")]
    pub(super) allow_overwrite: bool,

    #[clap(flatten)]
    compile_options: CompileOptions,

    /// Just show the hash of each packages, without actually performing the check.
    #[clap(long, hide = true)]
    show_program_hash: bool,
}

impl WorkspaceCommand for CheckCommand {
    fn package_selection(&self) -> PackageSelection {
        self.package_options.package_selection()
    }
    fn lock_type(&self) -> LockType {
        // Creates a `Prover.toml` template if it doesn't exist, otherwise only writes if `allow_overwrite` is true,
        // so it shouldn't lead to accidental conflicts. Doesn't produce compilation artifacts.
        LockType::None
    }
}

pub(crate) fn run(args: CheckCommand, workspace: Workspace) -> Result<(), CliError> {
    let mut workspace_file_manager = workspace.new_file_manager();
    insert_all_files_for_workspace_into_file_manager(&workspace, &mut workspace_file_manager);
    let parsed_files = parse_all(&workspace_file_manager);

    for package in &workspace {
        if args.show_program_hash {
            let (mut context, crate_id) =
                prepare_package(&workspace_file_manager, &parsed_files, package);
            check_crate(&mut context, crate_id, &args.compile_options).unwrap();
            let Some(main) = context.get_main_function(&crate_id) else {
                continue;
            };
            let program = monomorphize(main, &mut context.def_interner, false).unwrap();
            let hash = fxhash::hash64(&program);
            println!("{}: {:x}", package.name, hash);
            continue;
        }

        check_package(
            &workspace_file_manager,
            &parsed_files,
            package,
            &args.compile_options,
            args.allow_overwrite,
        )?;
    }
    Ok(())
}

/// Evaluates the necessity to create or update Prover.toml and Verifier.toml based on the allow_overwrite flag and files' existence.
/// Returns `true` if any file was generated or updated, `false` otherwise.
fn check_package(
    file_manager: &FileManager,
    parsed_files: &ParsedFiles,
    package: &Package,
    compile_options: &CompileOptions,
    allow_overwrite: bool,
) -> Result<(), CompileError> {
    let (mut context, crate_id) = prepare_package(file_manager, parsed_files, package);
    check_crate_and_report_errors(&mut context, crate_id, compile_options)?;

    if package.is_library() || package.is_contract() {
        // Libraries do not have ABIs while contracts have many, so we cannot generate a `Prover.toml` file.
        Ok(())
    } else {
        // XXX: We can have a --overwrite flag to determine if you want to overwrite the Prover/Verifier.toml files
        if let Some((parameters, _)) = compute_function_abi(&context, &crate_id) {
            let path_to_prover_input = package.prover_input_path();

            // Before writing the file, check if it exists and whether overwrite is set
            let should_write_prover = !path_to_prover_input.exists() || allow_overwrite;

            if should_write_prover {
                let prover_toml = create_input_toml_template(parameters.clone(), None);
                write_to_file(prover_toml.as_bytes(), &path_to_prover_input)
                    .expect("failed to write template");
            } else {
                eprintln!("Note: Prover.toml already exists. Use --overwrite to force overwrite.");
            }

            Ok(())
        } else {
            Err(CompileError::MissingMainFunction(package.name.clone()))
        }
    }
}

/// Generates the contents of a toml file with fields for each of the passed parameters.
fn create_input_toml_template(
    parameters: Vec<AbiParameter>,
    return_type: Option<AbiType>,
) -> String {
    /// Returns a default placeholder `toml::Value` for `typ` which
    /// complies with the structure of the specified `AbiType`.
    fn default_value(typ: AbiType) -> toml::Value {
        match typ {
            AbiType::Array { length, typ } => {
                let default_value_vec =
                    std::iter::repeat_n(default_value(*typ), length.try_into().unwrap()).collect();
                toml::Value::Array(default_value_vec)
            }
            AbiType::Struct { fields, .. } => {
                let default_value_map = toml::map::Map::from_iter(
                    fields.into_iter().map(|(name, typ)| (name, default_value(typ))),
                );
                toml::Value::Table(default_value_map)
            }
            _ => toml::Value::String("".to_owned()),
        }
    }

    let mut map =
        btree_map(parameters, |AbiParameter { name, typ, .. }| (name, default_value(typ)));

    if let Some(typ) = return_type {
        map.insert(MAIN_RETURN_NAME.to_owned(), default_value(typ));
    }

    toml::to_string(&map).unwrap()
}

#[cfg(test)]
mod tests {
    use noirc_abi::{AbiParameter, AbiType, AbiVisibility, Sign};

    use super::create_input_toml_template;

    #[test]
    fn valid_toml_template() {
        let typed_param = |name: &str, typ: AbiType| AbiParameter {
            name: name.to_string(),
            typ,
            visibility: AbiVisibility::Public,
        };
        let parameters = vec![
            typed_param("a", AbiType::Field),
            typed_param("b", AbiType::Integer { sign: Sign::Unsigned, width: 32 }),
            typed_param("c", AbiType::Array { length: 2, typ: Box::new(AbiType::Field) }),
            typed_param(
                "d",
                AbiType::Struct {
                    path: String::from("MyStruct"),
                    fields: vec![
                        (String::from("d1"), AbiType::Field),
                        (
                            String::from("d2"),
                            AbiType::Array { length: 3, typ: Box::new(AbiType::Field) },
                        ),
                    ],
                },
            ),
            typed_param("e", AbiType::Boolean),
        ];

        let toml_str = create_input_toml_template(parameters, None);

        let expected_toml_str = r#"a = ""
b = ""
c = ["", ""]
e = ""

[d]
d1 = ""
d2 = ["", "", ""]
"#;
        assert_eq!(toml_str, expected_toml_str);
    }
}
