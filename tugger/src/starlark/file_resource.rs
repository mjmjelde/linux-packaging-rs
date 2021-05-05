// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use {
    crate::starlark::code_signing::{handle_signable_event, SigningAction, SigningContext},
    slog::warn,
    starlark::{
        environment::TypeValues,
        eval::call_stack::CallStack,
        values::{
            error::{RuntimeError, ValueError},
            none::NoneType,
            {Mutable, TypedValue, Value, ValueResult},
        },
        {
            starlark_fun, starlark_module, starlark_parse_param_type, starlark_signature,
            starlark_signature_extraction, starlark_signatures,
        },
    },
    starlark_dialect_build_targets::{
        get_context_value, optional_list_arg, optional_str_arg, required_list_arg,
        EnvironmentContext, ResolvedTarget, ResolvedTargetValue, RunMode,
    },
    std::{
        collections::HashSet,
        convert::TryFrom,
        path::{Path, PathBuf},
    },
    tugger_code_signing::SigningDestination,
    tugger_common::glob::evaluate_glob,
    tugger_file_manifest::{FileEntry, FileManifest},
};

fn error_context<F, T>(label: &str, f: F) -> Result<T, ValueError>
where
    F: FnOnce() -> anyhow::Result<T>,
{
    f().map_err(|e| {
        ValueError::Runtime(RuntimeError {
            code: "TUGGER_FILE_RESOURCE",
            message: format!("{:?}", e),
            label: label.to_string(),
        })
    })
}

/// Run signing checks after a FileManifest has been materialized.
fn post_materialize_signing_checks(
    label: &'static str,
    type_values: &TypeValues,
    call_stack: &mut CallStack,
    action: SigningAction,
    installed_paths: &[PathBuf],
) -> Result<(), ValueError> {
    for path in installed_paths {
        let filename = path.file_name().ok_or_else(|| {
            ValueError::Runtime(RuntimeError {
                code: "TUGGER_FILE_RESOURCE",
                message: "unable to resolve filename of path (this should never happen)"
                    .to_string(),
                label: label.to_string(),
            })
        })?;

        let candidate = path.as_path().into();
        let mut context = SigningContext::new(label, action, filename, &candidate);
        context.set_path(&path);
        context.set_signing_destination(SigningDestination::File(path.clone()));

        handle_signable_event(type_values, call_stack, context)?;
    }

    Ok(())
}

#[derive(Clone, Debug)]
pub struct FileManifestValue {
    pub manifest: FileManifest,
    /// Optional path to be the default run target.
    pub run_path: Option<PathBuf>,
}

impl TypedValue for FileManifestValue {
    type Holder = Mutable<FileManifestValue>;
    const TYPE: &'static str = "FileManifest";

    fn values_for_descendant_check_and_freeze(&self) -> Box<dyn Iterator<Item = Value>> {
        Box::new(std::iter::empty())
    }
}

// Starlark functions.
impl FileManifestValue {
    /// FileManifest()
    pub fn new_from_args() -> ValueResult {
        let manifest = FileManifest::default();

        Ok(Value::new(FileManifestValue {
            manifest,
            run_path: None,
        }))
    }

    fn build(
        &self,
        type_values: &TypeValues,
        call_stack: &mut CallStack,
        target: String,
    ) -> ValueResult {
        const LABEL: &str = "FileManifest.build()";

        let context_value = get_context_value(type_values)?;
        let context = context_value
            .downcast_ref::<EnvironmentContext>()
            .ok_or(ValueError::IncorrectParameterType)?;

        let output_path = context.target_build_path(&target);

        let installed_paths = error_context("FileManifest.build()", || {
            warn!(
                context.logger(),
                "installing files to {}",
                output_path.display()
            );
            self.manifest
                .materialize_files_with_replace(&output_path)
                .map_err(anyhow::Error::new)
        })?;

        post_materialize_signing_checks(
            LABEL,
            type_values,
            call_stack,
            SigningAction::FileManifestInstall,
            &installed_paths,
        )?;

        // Use the stored run target if available, falling back to the single
        // executable file if non-ambiguous.
        // TODO support defining default run target in data structure.
        let run_mode = if let Some(default) = &self.run_path {
            RunMode::Path {
                path: output_path.join(default),
            }
        } else {
            let exes = self
                .manifest
                .iter_entries()
                .filter(|(_, c)| c.executable)
                .collect::<Vec<_>>();

            if exes.len() == 1 {
                RunMode::Path {
                    path: output_path.join(exes[0].0),
                }
            } else {
                RunMode::None
            }
        };

        Ok(Value::new(ResolvedTargetValue {
            inner: ResolvedTarget {
                run_mode,
                output_path,
            },
        }))
    }

    /// FileManifest.add_manifest(other)
    pub fn add_manifest(&mut self, other: FileManifestValue) -> ValueResult {
        error_context("FileManifest.add_manifest()", || {
            self.manifest
                .add_manifest(&other.manifest)
                .map_err(anyhow::Error::new)
        })?;

        Ok(Value::new(NoneType::None))
    }

    /// FileManifest.add_path(path, strip_prefix, force_read=False)
    pub fn add_path(
        &mut self,
        path: String,
        strip_prefix: String,
        force_read: bool,
    ) -> ValueResult {
        error_context("FileManifest.add_path()", || {
            let path = Path::new(&path);
            let strip_prefix = Path::new(&strip_prefix);

            if force_read {
                self.manifest.add_path_memory(path, strip_prefix)
            } else {
                self.manifest.add_path(path, strip_prefix)
            }
            .map_err(anyhow::Error::new)
        })?;

        Ok(Value::new(NoneType::None))
    }

    /// FileManifest.install(path, replace=true)
    pub fn install(
        &self,
        type_values: &TypeValues,
        call_stack: &mut CallStack,
        path: String,
        replace: bool,
    ) -> ValueResult {
        const LABEL: &str = "FileManifest.install()";

        let raw_context = get_context_value(type_values)?;
        let context = raw_context
            .downcast_ref::<EnvironmentContext>()
            .ok_or(ValueError::IncorrectParameterType)?;

        let installed_paths = error_context(LABEL, || {
            let dest_path = context.build_path().join(path);

            if replace {
                self.manifest.materialize_files_with_replace(&dest_path)
            } else {
                self.manifest.materialize_files(&dest_path)
            }
            .map_err(anyhow::Error::new)
        })?;

        post_materialize_signing_checks(
            LABEL,
            type_values,
            call_stack,
            SigningAction::FileManifestInstall,
            &installed_paths,
        )?;

        Ok(Value::new(NoneType::None))
    }
}

/// glob(include, exclude=None, relative_to=None)
fn starlark_glob(
    type_values: &TypeValues,
    include: &Value,
    exclude: &Value,
    strip_prefix: &Value,
) -> ValueResult {
    required_list_arg("include", "string", include)?;
    optional_list_arg("exclude", "string", exclude)?;
    let strip_prefix = optional_str_arg("strip_prefix", strip_prefix)?;

    let include = include
        .iter()?
        .iter()
        .map(|x| x.to_string())
        .collect::<Vec<String>>();

    let exclude = match exclude.get_type() {
        "list" => exclude.iter()?.iter().map(|x| x.to_string()).collect(),
        _ => Vec::new(),
    };

    let raw_context = get_context_value(type_values)?;
    let context = raw_context
        .downcast_ref::<EnvironmentContext>()
        .ok_or(ValueError::IncorrectParameterType)?;

    let manifest = error_context("glob()", || {
        let mut result = HashSet::new();

        // Evaluate all the includes first.
        for v in include {
            for p in evaluate_glob(context.cwd(), &v)? {
                result.insert(p);
            }
        }

        // Then apply excludes.
        for v in exclude {
            for p in evaluate_glob(context.cwd(), &v)? {
                result.remove(&p);
            }
        }

        let mut manifest = FileManifest::default();

        for path in result {
            let content = FileEntry::try_from(path.as_path())?;

            let path = if let Some(prefix) = &strip_prefix {
                path.strip_prefix(prefix)?.to_path_buf()
            } else {
                path.to_path_buf()
            };

            manifest.add_file_entry(&path, content)?;
        }

        Ok(manifest)
    })?;

    Ok(Value::new(FileManifestValue {
        manifest,
        run_path: None,
    }))
}

starlark_module! { file_resource_module =>
    glob(env env, include, exclude=NoneType::None, strip_prefix=NoneType::None) {
        starlark_glob(&env, &include, &exclude, &strip_prefix)
    }

    #[allow(non_snake_case)]
    FileManifest(env _env) {
        FileManifestValue::new_from_args()
    }

    FileManifest.add_manifest(this, other: FileManifestValue) {
        let mut this = this.downcast_mut::<FileManifestValue>().unwrap().unwrap();
        this.add_manifest(other)
    }

    FileManifest.add_path(this, path: String, strip_prefix: String, force_read: bool = false) {
        let mut this = this.downcast_mut::<FileManifestValue>().unwrap().unwrap();
        this.add_path(path, strip_prefix, force_read)
    }

    FileManifest.build(env env, call_stack cs, this, target: String) {
        let this = this.downcast_ref::<FileManifestValue>().unwrap();
        this.build(env, cs, target)
    }

    FileManifest.install(env env, call_stack cs, this, path: String, replace: bool = true) {
        let this = this.downcast_ref::<FileManifestValue>().unwrap();
        this.install(env, cs, path, replace)
    }
}

#[cfg(test)]
mod tests {
    use {super::*, crate::starlark::testutil::*, anyhow::Result, tugger_common::testutil::*};

    #[test]
    fn test_new_file_manifest() {
        let m = starlark_ok("FileManifest()");
        assert_eq!(m.get_type(), "FileManifest");

        let m = m.downcast_ref::<FileManifestValue>().unwrap();
        assert_eq!(m.manifest, FileManifest::default());
    }

    #[test]
    fn test_add_file_manifest() -> Result<()> {
        let mut env = StarlarkEnvironment::new()?;
        env.eval("m1 = FileManifest()")?;
        env.eval("m2 = FileManifest()")?;

        env.eval("m1.add_manifest(m2)")?;

        Ok(())
    }

    #[test]
    fn test_add_path() -> Result<()> {
        let mut env = StarlarkEnvironment::new()?;
        let manifest_value = env.eval("m = FileManifest(); m")?;

        let res = env.eval("m.add_path('/does/not/exist', '/does/not')");
        assert!(res.is_err());

        let temp_file0 = DEFAULT_TEMP_DIR.path().join("test_add_path_0");
        let temp_file1 = DEFAULT_TEMP_DIR.path().join("test_add_path_1");
        std::fs::write(&temp_file0, vec![42])?;
        std::fs::write(&temp_file1, vec![42, 42])?;
        let parent = temp_file0.parent().unwrap();

        env.eval(&format!(
            "m.add_path('{}', '{}')",
            temp_file0.display().to_string().escape_default(),
            parent.display().to_string().escape_default()
        ))?;
        env.eval(&format!(
            "m.add_path('{}', '{}', force_read = True)",
            temp_file1.display().to_string().escape_default(),
            parent.display().to_string().escape_default()
        ))?;

        let manifest = manifest_value.downcast_ref::<FileManifestValue>().unwrap();
        assert_eq!(manifest.manifest.iter_files().count(), 2);
        assert_eq!(
            manifest.manifest.get("test_add_path_0"),
            Some(&FileEntry {
                executable: false,
                data: temp_file0.into(),
            })
        );
        assert_eq!(
            manifest.manifest.get("test_add_path_1"),
            Some(&FileEntry {
                executable: false,
                data: vec![42, 42].into(),
            })
        );

        Ok(())
    }
}
