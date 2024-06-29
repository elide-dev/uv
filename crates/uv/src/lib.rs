use std::env;
use std::ffi::OsString;
use std::fmt::Write;
use std::io::stdout;
use std::path::PathBuf;

use anstream::eprintln;
use owo_colors::OwoColorize;

use anyhow::Result;
use clap::error::{ContextKind, ContextValue};
use clap::{CommandFactory, Parser};
use tracing::{debug, instrument};

use settings::PipTreeSettings;
use uv_cache::Cache;
use uv_cli::{
    compat::CompatArgs, CacheCommand, CacheNamespace, Cli, Commands, PipCommand, PipNamespace,
    ProjectCommand,
};
#[cfg(feature = "self-update")]
use uv_cli::{SelfCommand, SelfNamespace};
use uv_cli::{ToolCommand, ToolNamespace, ToolchainCommand, ToolchainNamespace};
use uv_configuration::Concurrency;
use uv_distribution::Workspace;
use uv_requirements::RequirementsSource;
use uv_settings::Combine;

use crate::commands::ExitStatus;
use crate::settings::{
    CacheSettings, GlobalSettings, PipCheckSettings, PipCompileSettings, PipFreezeSettings,
    PipInstallSettings, PipListSettings, PipShowSettings, PipSyncSettings, PipUninstallSettings,
};

pub mod commands;
pub mod logging;
pub mod printer;
pub mod settings;
pub mod shell;
pub mod version;

/// Run the main entrypoint for Uv.
pub fn run_main() -> ExitStatus {
    let result = if let Ok(stack_size) = env::var("UV_STACK_SIZE") {
        // Artificially limit the stack size to test for stack overflows. Windows has a default stack size of 1MB,
        // which is lower than the linux and mac default.
        // https://learn.microsoft.com/en-us/cpp/build/reference/stack-stack-allocations?view=msvc-170
        let stack_size = stack_size.parse().expect("Invalid stack size");
        let tokio_main = move || {
            tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .thread_stack_size(stack_size)
                .build()
                .expect("Failed building the Runtime")
                .block_on(run_uv_os_args())
        };
        std::thread::Builder::new()
            .stack_size(stack_size)
            .spawn(tokio_main)
            .expect("Tokio executor failed, was there a panic?")
            .join()
            .expect("Tokio executor failed, was there a panic?")
    } else {
        tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("Failed building the Runtime")
            .block_on(run_uv_os_args())
    };

    match result {
        Ok(code) => code.into(),
        Err(err) => {
            let mut causes = err.chain();
            eprintln!("{}: {}", "error".red().bold(), causes.next().unwrap());
            for err in causes {
                eprintln!("  {}: {}", "Caused by".red().bold(), err);
            }
            ExitStatus::Error.into()
        }
    }
}

#[inline(always)]
pub async fn run_uv_os_args() -> Result<ExitStatus> {
    run_uv_entry(None).await
}

#[cfg(feature = "logging")]
fn do_setup_logging(globals: &GlobalSettings) {
    #[cfg(feature = "tracing-durations-export")]
    let (duration_layer, _duration_guard) = logging::setup_duration()?;
    #[cfg(not(feature = "tracing-durations-export"))]
    let duration_layer = None::<tracing_subscriber::layer::Identity>;
    logging::setup_logging(
        match globals.verbose {
            0 => logging::Level::Default,
            1 => logging::Level::Verbose,
            2.. => logging::Level::ExtraVerbose,
        },
        duration_layer,
    ).expect("Failed to setup logging");
}

#[cfg(feature = "logging")]
fn setup_logging(globals: &GlobalSettings) {
    do_setup_logging(globals)
}

#[instrument]
pub async fn run_uv_entry(args: Option<Vec<OsString>>) -> Result<ExitStatus> {
    let cli = match args {
        Some(args) => Cli::try_parse_from(args),
        None => Cli::try_parse()
    };
    let cli = match cli {
        Ok(cli) => cli,
        Err(mut err) => {
            if let Some(ContextValue::String(subcommand)) = err.get(ContextKind::InvalidSubcommand)
            {
                match subcommand.as_str() {
                    "compile" | "lock" => {
                        err.insert(
                            ContextKind::SuggestedSubcommand,
                            ContextValue::String("uv pip compile".to_string()),
                        );
                    }
                    "sync" => {
                        err.insert(
                            ContextKind::SuggestedSubcommand,
                            ContextValue::String("uv pip sync".to_string()),
                        );
                    }
                    "install" | "add" => {
                        err.insert(
                            ContextKind::SuggestedSubcommand,
                            ContextValue::String("uv pip install".to_string()),
                        );
                    }
                    "uninstall" | "remove" => {
                        err.insert(
                            ContextKind::SuggestedSubcommand,
                            ContextValue::String("uv pip uninstall".to_string()),
                        );
                    }
                    "freeze" => {
                        err.insert(
                            ContextKind::SuggestedSubcommand,
                            ContextValue::String("uv pip freeze".to_string()),
                        );
                    }
                    "list" => {
                        err.insert(
                            ContextKind::SuggestedSubcommand,
                            ContextValue::String("uv pip list".to_string()),
                        );
                    }
                    "show" => {
                        err.insert(
                            ContextKind::SuggestedSubcommand,
                            ContextValue::String("uv pip show".to_string()),
                        );
                    }
                    "tree" => {
                        err.insert(
                            ContextKind::SuggestedSubcommand,
                            ContextValue::String("uv pip tree".to_string()),
                        );
                    }
                    _ => {}
                }
            }
            err.exit()
        }
    };

    // enable flag to pick up warnings generated by workspace loading.
    if !cli.global_args.quiet {
        uv_warnings::enable();
    }

    // Load configuration from the filesystem, prioritizing (in order):
    // 1. The configuration file specified on the command-line.
    // 2. The configuration file in the current workspace (i.e., the `pyproject.toml` or `uv.toml`
    //    file in the workspace root directory). If found, this file is combined with the user
    //    configuration file.
    // 3. The nearest `uv.toml` file in the directory tree, starting from the current directory. If
    //    found, this file is combined with the user configuration file. In this case, we don't
    //    search for `pyproject.toml` files, since we're not in a workspace.
    let filesystem = if let Some(config_file) = cli.config_file.as_ref() {
        Some(uv_settings::FilesystemOptions::from_file(config_file)?)
    } else if cli.global_args.isolated {
        None
    } else if let Ok(project) = Workspace::discover(&env::current_dir()?, None).await {
        let project = uv_settings::FilesystemOptions::from_directory(project.root())?;
        let user = uv_settings::FilesystemOptions::user()?;
        project.combine(user)
    } else {
        let project = uv_settings::FilesystemOptions::find(env::current_dir()?)?;
        let user = uv_settings::FilesystemOptions::user()?;
        project.combine(user)
    };

    // Resolve the global settings.
    let globals = GlobalSettings::resolve(&cli.command, &cli.global_args, filesystem.as_ref());

    // Resolve the cache settings.
    let cache_settings = CacheSettings::resolve(cli.cache_args, filesystem.as_ref());

    // Configure the `tracing` crate, which controls internal logging.
    #[cfg(feature = "logging")]
    setup_logging(&globals);

    // Configure the `Printer`, which controls user-facing output in the CLI.
    let printer = if globals.quiet {
        printer::Printer::Quiet
    } else if globals.verbose > 0 {
        printer::Printer::Verbose
    } else {
        printer::Printer::Default
    };

    // Configure the `warn!` macros, which control user-facing warnings in the CLI.
    if globals.quiet {
        uv_warnings::disable();
    } else {
        uv_warnings::enable();
    }

    anstream::ColorChoice::write_global(globals.color.into());

    miette::set_hook(Box::new(|_| {
        Box::new(
            miette::MietteHandlerOpts::new()
                .break_words(false)
                .word_separator(textwrap::WordSeparator::AsciiSpace)
                .word_splitter(textwrap::WordSplitter::NoHyphenation)
                .wrap_lines(env::var("UV_NO_WRAP").map(|_| false).unwrap_or(true))
                .build(),
        )
    }))?;

    debug!("uv {}", version::version());

    // Write out any resolved settings.
    macro_rules! show_settings {
        ($arg:expr) => {
            if globals.show_settings {
                writeln!(printer.stdout(), "{:#?}", $arg)?;
                return Ok(ExitStatus::Success);
            }
        };
        ($arg:expr, false) => {
            if globals.show_settings {
                writeln!(printer.stdout(), "{:#?}", $arg)?;
            }
        };
    }
    show_settings!(globals, false);
    show_settings!(cache_settings, false);

    // Configure the cache.
    let cache = Cache::from_settings(cache_settings.no_cache, cache_settings.cache_dir)?;

    match cli.command {
        Commands::Pip(PipNamespace {
                          command: PipCommand::Compile(args),
                      }) => {
            args.compat_args.validate()?;

            // Resolve the settings from the command-line arguments and workspace configuration.
            let args = PipCompileSettings::resolve(args, filesystem);
            show_settings!(args);

            rayon::ThreadPoolBuilder::new()
                .num_threads(args.settings.concurrency.installs)
                .build_global()
                .expect("failed to initialize global rayon pool");

            // Initialize the cache.
            let cache = cache.init()?.with_refresh(args.refresh);

            let requirements = args
                .src_file
                .into_iter()
                .map(RequirementsSource::from_requirements_file)
                .collect::<Vec<_>>();
            let constraints = args
                .constraint
                .into_iter()
                .map(RequirementsSource::from_constraints_txt)
                .collect::<Vec<_>>();
            let overrides = args
                .r#override
                .into_iter()
                .map(RequirementsSource::from_overrides_txt)
                .collect::<Vec<_>>();

            commands::pip_compile(
                &requirements,
                &constraints,
                &overrides,
                args.overrides_from_workspace,
                args.settings.extras,
                args.settings.output_file.as_deref(),
                args.settings.resolution,
                args.settings.prerelease,
                args.settings.dependency_mode,
                args.settings.upgrade,
                args.settings.generate_hashes,
                args.settings.no_emit_package,
                args.settings.no_strip_extras,
                args.settings.no_strip_markers,
                !args.settings.no_annotate,
                !args.settings.no_header,
                args.settings.custom_compile_command,
                args.settings.emit_index_url,
                args.settings.emit_find_links,
                args.settings.emit_build_options,
                args.settings.emit_marker_expression,
                args.settings.emit_index_annotation,
                args.settings.index_locations,
                args.settings.index_strategy,
                args.settings.keyring_provider,
                args.settings.setup_py,
                args.settings.config_setting,
                globals.connectivity,
                args.settings.no_build_isolation,
                args.settings.build_options,
                args.settings.python_version,
                args.settings.python_platform,
                args.settings.universal,
                args.settings.exclude_newer,
                args.settings.annotation_style,
                args.settings.link_mode,
                args.settings.python,
                args.settings.system,
                globals.toolchain_preference,
                args.settings.concurrency,
                globals.native_tls,
                globals.quiet,
                globals.preview,
                cache,
                printer,
            )
                .await
        }
        Commands::Pip(PipNamespace {
                          command: PipCommand::Sync(args),
                      }) => {
            args.compat_args.validate()?;

            // Resolve the settings from the command-line arguments and workspace configuration.
            let args = PipSyncSettings::resolve(args, filesystem);
            show_settings!(args);

            rayon::ThreadPoolBuilder::new()
                .num_threads(args.settings.concurrency.installs)
                .build_global()
                .expect("failed to initialize global rayon pool");

            // Initialize the cache.
            let cache = cache.init()?.with_refresh(args.refresh);

            let requirements = args
                .src_file
                .into_iter()
                .map(RequirementsSource::from_requirements_file)
                .collect::<Vec<_>>();
            let constraints = args
                .constraint
                .into_iter()
                .map(RequirementsSource::from_constraints_txt)
                .collect::<Vec<_>>();

            commands::pip_sync(
                &requirements,
                &constraints,
                args.settings.reinstall,
                args.settings.link_mode,
                args.settings.compile_bytecode,
                args.settings.require_hashes,
                args.settings.index_locations,
                args.settings.index_strategy,
                args.settings.keyring_provider,
                args.settings.setup_py,
                globals.connectivity,
                &args.settings.config_setting,
                args.settings.no_build_isolation,
                args.settings.build_options,
                args.settings.python_version,
                args.settings.python_platform,
                args.settings.strict,
                args.settings.exclude_newer,
                args.settings.python,
                args.settings.system,
                args.settings.break_system_packages,
                args.settings.target,
                args.settings.prefix,
                args.settings.concurrency,
                globals.native_tls,
                globals.preview,
                cache,
                args.dry_run,
                printer,
            )
                .await
        }
        Commands::Pip(PipNamespace {
                          command: PipCommand::Install(args),
                      }) => {
            args.compat_args.validate()?;

            // Resolve the settings from the command-line arguments and workspace configuration.
            let args = PipInstallSettings::resolve(args, filesystem);
            show_settings!(args);

            rayon::ThreadPoolBuilder::new()
                .num_threads(args.settings.concurrency.installs)
                .build_global()
                .expect("failed to initialize global rayon pool");

            // Initialize the cache.
            let cache = cache.init()?.with_refresh(args.refresh);
            let requirements = args
                .package
                .into_iter()
                .map(RequirementsSource::from_package)
                .chain(args.editable.into_iter().map(RequirementsSource::Editable))
                .chain(
                    args.requirement
                        .into_iter()
                        .map(RequirementsSource::from_requirements_file),
                )
                .collect::<Vec<_>>();
            let constraints = args
                .constraint
                .into_iter()
                .map(RequirementsSource::from_constraints_txt)
                .collect::<Vec<_>>();
            let overrides = args
                .r#override
                .into_iter()
                .map(RequirementsSource::from_overrides_txt)
                .collect::<Vec<_>>();

            commands::pip_install(
                &requirements,
                &constraints,
                &overrides,
                args.overrides_from_workspace,
                &args.settings.extras,
                args.settings.resolution,
                args.settings.prerelease,
                args.settings.dependency_mode,
                args.settings.upgrade,
                args.settings.index_locations,
                args.settings.index_strategy,
                args.settings.keyring_provider,
                args.settings.reinstall,
                args.settings.link_mode,
                args.settings.compile_bytecode,
                args.settings.require_hashes,
                args.settings.setup_py,
                globals.connectivity,
                &args.settings.config_setting,
                args.settings.no_build_isolation,
                args.settings.build_options,
                args.settings.python_version,
                args.settings.python_platform,
                args.settings.strict,
                args.settings.exclude_newer,
                args.settings.python,
                args.settings.system,
                args.settings.break_system_packages,
                args.settings.target,
                args.settings.prefix,
                args.settings.concurrency,
                globals.native_tls,
                globals.preview,
                cache,
                args.dry_run,
                printer,
            )
                .await
        }
        Commands::Pip(PipNamespace {
                          command: PipCommand::Uninstall(args),
                      }) => {
            // Resolve the settings from the command-line arguments and workspace configuration.
            let args = PipUninstallSettings::resolve(args, filesystem);
            show_settings!(args);

            // Initialize the cache.
            let cache = cache.init()?;

            let sources = args
                .package
                .into_iter()
                .map(RequirementsSource::from_package)
                .chain(
                    args.requirement
                        .into_iter()
                        .map(RequirementsSource::from_requirements_txt),
                )
                .collect::<Vec<_>>();
            commands::pip_uninstall(
                &sources,
                args.settings.python,
                args.settings.system,
                args.settings.break_system_packages,
                args.settings.target,
                args.settings.prefix,
                cache,
                globals.connectivity,
                globals.native_tls,
                globals.preview,
                args.settings.keyring_provider,
                printer,
            )
                .await
        }
        Commands::Pip(PipNamespace {
                          command: PipCommand::Freeze(args),
                      }) => {
            // Resolve the settings from the command-line arguments and workspace configuration.
            let args = PipFreezeSettings::resolve(args, filesystem);
            show_settings!(args);

            // Initialize the cache.
            let cache = cache.init()?;

            commands::pip_freeze(
                args.exclude_editable,
                args.settings.strict,
                args.settings.python.as_deref(),
                args.settings.system,
                globals.preview,
                &cache,
                printer,
            )
        }
        Commands::Pip(PipNamespace {
                          command: PipCommand::List(args),
                      }) => {
            args.compat_args.validate()?;

            // Resolve the settings from the command-line arguments and workspace configuration.
            let args = PipListSettings::resolve(args, filesystem);
            show_settings!(args);

            // Initialize the cache.
            let cache = cache.init()?;

            commands::pip_list(
                args.editable,
                args.exclude_editable,
                &args.exclude,
                &args.format,
                args.settings.strict,
                args.settings.python.as_deref(),
                args.settings.system,
                globals.preview,
                &cache,
                printer,
            )
        }
        Commands::Pip(PipNamespace {
                          command: PipCommand::Show(args),
                      }) => {
            // Resolve the settings from the command-line arguments and workspace configuration.
            let args = PipShowSettings::resolve(args, filesystem);
            show_settings!(args);

            // Initialize the cache.
            let cache = cache.init()?;

            commands::pip_show(
                args.package,
                args.settings.strict,
                args.settings.python.as_deref(),
                args.settings.system,
                globals.preview,
                &cache,
                printer,
            )
        }
        Commands::Pip(PipNamespace {
                          command: PipCommand::Tree(args),
                      }) => {
            // Resolve the settings from the command-line arguments and workspace configuration.
            let args = PipTreeSettings::resolve(args, filesystem);

            // Initialize the cache.
            let cache = cache.init()?;

            commands::pip_tree(
                args.depth,
                args.prune,
                args.no_dedupe,
                args.shared.strict,
                args.shared.python.as_deref(),
                args.shared.system,
                globals.preview,
                &cache,
                printer,
            )
        }
        Commands::Pip(PipNamespace {
                          command: PipCommand::Check(args),
                      }) => {
            // Resolve the settings from the command-line arguments and workspace configuration.
            let args = PipCheckSettings::resolve(args, filesystem);
            show_settings!(args);

            // Initialize the cache.
            let cache = cache.init()?;

            commands::pip_check(
                args.settings.python.as_deref(),
                args.settings.system,
                globals.preview,
                &cache,
                printer,
            )
        }
        Commands::Cache(CacheNamespace {
                            command: CacheCommand::Clean(args),
                        })
        | Commands::Clean(args) => {
            show_settings!(args);
            commands::cache_clean(&args.package, &cache, printer)
        }
        Commands::Cache(CacheNamespace {
                            command: CacheCommand::Prune,
                        }) => commands::cache_prune(&cache, printer),
        Commands::Cache(CacheNamespace {
                            command: CacheCommand::Dir,
                        }) => {
            commands::cache_dir(&cache);
            Ok(ExitStatus::Success)
        }
        Commands::Venv(args) => {
            args.compat_args.validate()?;

            // Resolve the settings from the command-line arguments and workspace configuration.
            let args = settings::VenvSettings::resolve(args, filesystem);
            show_settings!(args);

            // Initialize the cache.
            let cache = cache.init()?;

            // Since we use ".venv" as the default name, we use "." as the default prompt.
            let prompt = args.prompt.or_else(|| {
                if args.name == PathBuf::from(".venv") {
                    Some(".".to_string())
                } else {
                    None
                }
            });

            commands::venv(
                &args.name,
                args.settings.python.as_deref(),
                globals.toolchain_preference,
                args.settings.link_mode,
                &args.settings.index_locations,
                args.settings.index_strategy,
                args.settings.keyring_provider,
                uv_virtualenv::Prompt::from_args(prompt),
                args.system_site_packages,
                globals.connectivity,
                args.seed,
                args.allow_existing,
                args.settings.exclude_newer,
                globals.native_tls,
                globals.preview,
                &cache,
                printer,
            )
                .await
        }
        Commands::Project(ProjectCommand::Run(args)) => {
            // Resolve the settings from the command-line arguments and workspace configuration.
            let args = settings::RunSettings::resolve(args, filesystem);
            show_settings!(args);

            // Initialize the cache.
            let cache = cache.init()?.with_refresh(args.refresh);

            let requirements = args
                .with
                .into_iter()
                .map(RequirementsSource::from_package)
                .collect::<Vec<_>>();

            commands::run(
                args.extras,
                args.dev,
                args.command,
                requirements,
                args.python,
                args.package,
                args.settings,
                globals.isolated,
                globals.preview,
                globals.toolchain_preference,
                globals.connectivity,
                Concurrency::default(),
                globals.native_tls,
                &cache,
                printer,
            )
                .await
        }
        Commands::Project(ProjectCommand::Sync(args)) => {
            // Resolve the settings from the command-line arguments and workspace configuration.
            let args = settings::SyncSettings::resolve(args, filesystem);
            show_settings!(args);

            // Initialize the cache.
            let cache = cache.init()?.with_refresh(args.refresh);

            commands::sync(
                args.extras,
                args.dev,
                args.modifications,
                args.python,
                globals.toolchain_preference,
                args.settings,
                globals.preview,
                globals.connectivity,
                Concurrency::default(),
                globals.native_tls,
                &cache,
                printer,
            )
                .await
        }
        Commands::Project(ProjectCommand::Lock(args)) => {
            // Resolve the settings from the command-line arguments and workspace configuration.
            let args = settings::LockSettings::resolve(args, filesystem);
            show_settings!(args);

            // Initialize the cache.
            let cache = cache.init()?.with_refresh(args.refresh);

            commands::lock(
                args.python,
                args.settings,
                globals.preview,
                globals.toolchain_preference,
                globals.connectivity,
                Concurrency::default(),
                globals.native_tls,
                &cache,
                printer,
            )
                .await
        }
        Commands::Project(ProjectCommand::Add(args)) => {
            // Resolve the settings from the command-line arguments and workspace configuration.
            let args = settings::AddSettings::resolve(args, filesystem);
            show_settings!(args);

            // Initialize the cache.
            let cache = cache.init()?.with_refresh(args.refresh);

            commands::add(
                args.requirements,
                args.editable,
                args.dependency_type,
                args.raw_sources,
                args.rev,
                args.tag,
                args.branch,
                args.extras,
                args.package,
                args.python,
                args.settings,
                globals.toolchain_preference,
                globals.preview,
                globals.connectivity,
                Concurrency::default(),
                globals.native_tls,
                &cache,
                printer,
            )
                .await
        }
        Commands::Project(ProjectCommand::Remove(args)) => {
            // Resolve the settings from the command-line arguments and workspace configuration.
            let args = settings::RemoveSettings::resolve(args, filesystem);
            show_settings!(args);

            // Initialize the cache.
            let cache = cache.init()?;

            commands::remove(
                args.requirements,
                args.dependency_type,
                args.package,
                args.python,
                globals.toolchain_preference,
                globals.preview,
                globals.connectivity,
                Concurrency::default(),
                globals.native_tls,
                &cache,
                printer,
            )
                .await
        }
        #[cfg(feature = "self-update")]
        Commands::Self_(SelfNamespace {
                            command: SelfCommand::Update,
                        }) => commands::self_update(printer).await,
        Commands::Version { output_format } => {
            commands::version(output_format, &mut stdout())?;
            Ok(ExitStatus::Success)
        }
        Commands::GenerateShellCompletion { shell } => {
            shell.generate(&mut Cli::command(), &mut stdout());
            Ok(ExitStatus::Success)
        }
        Commands::Tool(ToolNamespace {
                           command: ToolCommand::Run(args),
                       }) => {
            // Resolve the settings from the command-line arguments and workspace configuration.
            let args = settings::ToolRunSettings::resolve(args, filesystem);
            show_settings!(args);

            // Initialize the cache.
            let cache = cache.init()?.with_refresh(args.refresh);

            commands::tool_run(
                args.command,
                args.python,
                args.from,
                args.with,
                args.settings,
                globals.isolated,
                globals.preview,
                globals.toolchain_preference,
                globals.connectivity,
                Concurrency::default(),
                globals.native_tls,
                &cache,
                printer,
            )
                .await
        }
        Commands::Tool(ToolNamespace {
                           command: ToolCommand::Install(args),
                       }) => {
            // Resolve the settings from the command-line arguments and workspace configuration.
            let args = settings::ToolInstallSettings::resolve(args, filesystem);
            show_settings!(args);

            // Initialize the cache.
            let cache = cache.init()?.with_refresh(args.refresh);

            commands::tool_install(
                args.package,
                args.from,
                args.python,
                args.with,
                args.force,
                args.settings,
                globals.preview,
                globals.toolchain_preference,
                globals.connectivity,
                Concurrency::default(),
                globals.native_tls,
                &cache,
                printer,
            )
                .await
        }
        Commands::Toolchain(ToolchainNamespace {
                                command: ToolchainCommand::List(args),
                            }) => {
            // Resolve the settings from the command-line arguments and workspace configuration.
            let args = settings::ToolchainListSettings::resolve(args, filesystem);
            show_settings!(args);

            // Initialize the cache.
            let cache = cache.init()?;

            commands::toolchain_list(
                args.kinds,
                args.all_versions,
                args.all_platforms,
                globals.toolchain_preference,
                globals.preview,
                &cache,
                printer,
            )
                .await
        }
        Commands::Toolchain(ToolchainNamespace {
                                command: ToolchainCommand::Install(args),
                            }) => {
            // Resolve the settings from the command-line arguments and workspace configuration.
            let args = settings::ToolchainInstallSettings::resolve(args, filesystem);
            show_settings!(args);

            // Initialize the cache.
            let cache = cache.init()?;

            commands::toolchain_install(
                args.targets,
                args.force,
                globals.native_tls,
                globals.connectivity,
                globals.preview,
                &cache,
                printer,
            )
                .await
        }
        Commands::Toolchain(ToolchainNamespace {
                                command: ToolchainCommand::Find(args),
                            }) => {
            // Resolve the settings from the command-line arguments and workspace configuration.
            let args = settings::ToolchainFindSettings::resolve(args, filesystem);

            // Initialize the cache.
            let cache = cache.init()?;

            commands::toolchain_find(
                args.request,
                globals.toolchain_preference,
                globals.preview,
                &cache,
                printer,
            )
                .await
        }
    }
}