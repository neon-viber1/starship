use super::{Context, Module, ModuleConfig};

use super::utils::directory::truncate;
use crate::configs::conda::CondaConfig;
use crate::formatter::StringFormatter;

/// Creates a module with the current Conda environment
///
/// Will display the Conda environment iff `$CONDA_DEFAULT_ENV` is set.
pub fn module<'a>(context: &'a Context) -> Option<Module<'a>> {
    // Reference implementation: https://github.com/denysdovhan/spaceship-prompt/blob/master/sections/conda.zsh
    let conda_prefix = context
        .get_env("CONDA_PREFIX")
        .filter(|p| !p.trim().is_empty());
    let mut conda_env = context.get_env("CONDA_DEFAULT_ENV").unwrap_or_default();
    if conda_env.trim().is_empty() {
        // Fall back to the active prefix (e.g. micromamba's nameless root env)
        conda_env = conda_prefix.clone()?;
    }

    // The root environment is the base environment, even when it is
    // reported as a path (as micromamba does).
    let is_root_prefix = |value: &str| {
        let trim = |s: &str| s.trim_end_matches(['/', '\\']).to_string();
        let value = trim(value);
        ["MAMBA_ROOT_PREFIX", "CONDA_ROOT"]
            .iter()
            .filter_map(|var| context.get_env(var))
            .any(|root| !root.trim().is_empty() && trim(&root) == value)
    };
    if conda_env != "base" && is_root_prefix(&conda_env) {
        conda_env = "base".to_string();
    }

    let mut module = context.new_module("conda");
    let config: CondaConfig = CondaConfig::try_load(module.config);

    if config.ignore_base && conda_env == "base" {
        return None;
    }
    if !context.detect_env_vars(&config.detect_env_vars) {
        return None;
    }

    let conda_env = truncate(&conda_env, config.truncation_length).unwrap_or(conda_env);

    let parsed = StringFormatter::new(config.format).and_then(|formatter| {
        formatter
            .map_meta(|variable, _| match variable {
                "symbol" => Some(config.symbol),
                _ => None,
            })
            .map_style(|variable| match variable {
                "style" => Some(Ok(config.style)),
                _ => None,
            })
            .map(|variable| match variable {
                "environment" => Some(Ok(conda_env.as_str())),
                _ => None,
            })
            .parse(None, Some(context))
    });

    module.set_segments(match parsed {
        Ok(segments) => segments,
        Err(error) => {
            log::warn!("Error in module `conda`:\n{error}");
            return None;
        }
    });

    Some(module)
}

#[cfg(test)]
mod tests {
    use crate::test::ModuleRenderer;
    use nu_ansi_term::Color;

    #[test]
    fn not_in_env() {
        let actual = ModuleRenderer::new("conda").collect();

        let expected = None;

        assert_eq!(expected, actual);
    }

    #[test]
    fn ignore_base() {
        let actual = ModuleRenderer::new("conda")
            .env("CONDA_DEFAULT_ENV", "base")
            .config(toml::toml! {
                [conda]
                ignore_base = true
            })
            .collect();

        let expected = None;

        assert_eq!(expected, actual);
    }

    #[test]
    fn micromamba_root_hidden_by_default() {
        let actual = ModuleRenderer::new("conda")
            .env("CONDA_PREFIX", "/tmp/mm")
            .env("MAMBA_ROOT_PREFIX", "/tmp/mm")
            .collect();
        assert_eq!(None, actual);
    }

    #[test]
    fn micromamba_root_shown_as_base() {
        let actual = ModuleRenderer::new("conda")
            .env("CONDA_PREFIX", "/tmp/mm")
            .env("MAMBA_ROOT_PREFIX", "/tmp/mm")
            .config(toml::toml! {
                [conda]
                ignore_base = false
            })
            .collect();
        let expected = Some(format!("via {} ", Color::Green.bold().paint("🅒 base")));
        assert_eq!(expected, actual);
    }

    #[test]
    fn micromamba_root_path_default_env() {
        let actual = ModuleRenderer::new("conda")
            .env("CONDA_DEFAULT_ENV", "/tmp/mm")
            .env("CONDA_PREFIX", "/tmp/mm")
            .env("MAMBA_ROOT_PREFIX", "/tmp/mm/")
            .collect();
        assert_eq!(None, actual);
    }

    #[test]
    fn ignore_pixi_envs() {
        let actual = ModuleRenderer::new("conda")
            .env("CONDA_DEFAULT_ENV", "my-env")
            .env("PIXI_ENVIRONMENT_NAME", "my-env")
            .collect();

        let expected = None;

        assert_eq!(expected, actual);
    }

    #[test]
    fn env_set() {
        let actual = ModuleRenderer::new("conda")
            .env("CONDA_DEFAULT_ENV", "astronauts")
            .collect();

        let expected = Some(format!(
            "via {} ",
            Color::Green.bold().paint("🅒 astronauts")
        ));

        assert_eq!(expected, actual);
    }

    #[test]
    fn truncate() {
        let actual = ModuleRenderer::new("conda")
            .env("CONDA_DEFAULT_ENV", "/some/really/long/and/really/annoying/path/that/shouldnt/be/displayed/fully/conda/my_env")
            .collect();

        let expected = Some(format!("via {} ", Color::Green.bold().paint("🅒 my_env")));

        assert_eq!(expected, actual);
    }
}
