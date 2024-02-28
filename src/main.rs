use anyhow::{anyhow, bail, Result};
use clap::Parser;
use indoc::indoc;
use serde::Deserialize;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::{
    fmt::Display,
    fs,
    fs::{File, Permissions},
    io::Write,
    path::{Path, PathBuf},
};

#[derive(Parser, Debug)]
#[command(version, about, long_about = None)]
struct Args {
    input_dir: PathBuf,
    #[arg(short, long)]
    output_dir: PathBuf,
}

#[derive(Deserialize)]
#[serde(rename_all = "kebab-case")]
struct Service {
    #[serde(rename = "type")]
    type_: ServiceType,
    up: Option<String>,
    run: Option<String>,
    finish: Option<String>,
    consumer_for: Option<String>,
    producer_for: Option<String>,
    pipeline_name: Option<String>,
    dependencies: Option<Vec<String>>,
    extensions: Option<Extensions>,
}

#[derive(Clone, Copy, Deserialize)]
#[serde(rename_all = "lowercase")]
enum ServiceType {
    OneShot,
    LongRun,
}

impl Display for ServiceType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let str = match self {
            Self::OneShot => "oneshot".to_string(),
            Self::LongRun => "longrun".to_string(),
        };
        write!(f, "{}", str)
    }
}

#[derive(Default, Deserialize)]
#[serde(rename_all = "kebab-case")]
struct Extensions {
    log: Option<Log>,
    restart: Option<Restart>,
}

#[derive(Deserialize)]
#[serde(rename_all = "kebab-case")]
struct Log {
    dir: PathBuf,
    /// s6 logging script; cf. https://skarnet.org/software/s6/s6-log.html
    /// s6-overlay defaults to: "n20 s1000000 T"
    script: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "kebab-case")]
struct Restart {
    on_failure: bool,
}

fn main() -> Result<()> {
    let args = Args::parse();
    let paths = fs::read_dir(args.input_dir)?;
    let mut services = Vec::new();
    for path in paths {
        let path = path?;
        let meta = path.metadata()?;
        if meta.is_file() {
            let name = path
                .file_name()
                .into_string()
                .map_err(|_| anyhow!("illegal file name"))?
                .trim_end_matches(".toml")
                .to_string();
            let file = fs::read_to_string(path.path())?;
            let service: Service = toml::from_str(&file)?;
            services.push((name, service));
        }
    }
    let user_contents_dir = args.output_dir.join("user").join("contents.d");
    let _ = fs::remove_dir_all(&user_contents_dir);
    fs::create_dir_all(&user_contents_dir)?;
    loop {
        let mut more_services = Vec::new();
        for (name, mut service) in services.drain(..) {
            let service_dir = args.output_dir.join(&name);
            fs::create_dir_all(&service_dir)?;
            // process extensions first, since they can mutate the service definition
            if let Some(ref ext) = service.extensions {
                if let Some(ref log) = ext.log {
                    match service.type_ {
                        ServiceType::OneShot => {
                            if service.up.is_some() {
                                bail!("extension `log` would clobber up for oneshots");
                            }
                            service.up = Some(log_up(
                                &log.dir,
                                log.script.clone(),
                                &service_dir.canonicalize()?.join("run"),
                            ));
                        }
                        ServiceType::LongRun => {
                            let pipeline_name = format!("{name}-with-logs");
                            let logger_name = format!("{name}-log");
                            if service.producer_for.is_some() {
                                bail!("extension `log` would clobber producer-for");
                            }
                            service.producer_for = Some(logger_name.clone());
                            more_services.push((
                                logger_name,
                                Service {
                                    type_: ServiceType::LongRun,
                                    up: None,
                                    run: Some(log_run(&log.dir, log.script.clone())),
                                    finish: None,
                                    consumer_for: Some(name.clone()),
                                    producer_for: None,
                                    pipeline_name: Some(pipeline_name),
                                    dependencies: service.dependencies.clone(),
                                    extensions: None,
                                },
                            ));
                        }
                    }
                }
                if let Some(ref restart) = ext.restart {
                    if !restart.on_failure {
                        if service.finish.is_some() {
                            bail!("extension `restart` would clobber finish");
                        }
                        service.finish = Some(no_restart_on_failure());
                    }
                }
            }
            // write out service definition
            fs::write(service_dir.join("type"), service.type_.to_string())?;
            if let Some(ref up) = service.up {
                fs::write(service_dir.join("up"), up)?;
            }
            if let Some(ref run) = service.run {
                let mut f = File::create(service_dir.join("run"))?;
                f.write_all(run.as_ref())?;
                #[cfg(unix)]
                f.set_permissions(Permissions::from_mode(0o755))?;
            }
            if let Some(ref finish) = service.finish {
                fs::write(service_dir.join("finish"), finish)?;
            }
            if let Some(ref consumer_for) = service.consumer_for {
                fs::write(service_dir.join("consumer-for"), consumer_for)?;
            }
            if let Some(ref producer_for) = service.producer_for {
                fs::write(service_dir.join("producer-for"), producer_for)?;
            }
            if let Some(ref pipeline_name) = service.pipeline_name {
                fs::write(service_dir.join("pipeline-name"), pipeline_name)?;
            }
            if let Some(ref deps) = service.dependencies {
                let deps_dir = service_dir.join("dependencies.d");
                fs::create_dir_all(&deps_dir)?;
                for dep in deps {
                    fs::write(deps_dir.join(dep), "")?;
                }
            }
            // only write this service to the user bundle if it's standalone,
            // or the last service in a pipeline
            if service.consumer_for.is_none() && service.producer_for.is_none() {
                fs::write(user_contents_dir.join(name), "")?;
            } else if service.producer_for.is_none() {
                if let Some(ref pipeline_name) = &service.pipeline_name {
                    fs::write(user_contents_dir.join(pipeline_name), "")?;
                } else {
                    println!("skipping {name}: service consumes another but has no pipeline name");
                }
            }
        }
        if more_services.is_empty() {
            break;
        }
        services = more_services;
    }
    Ok(())
}

fn log_run(path: &Path, script: Option<String>) -> String {
    let script = match script {
        Some(s) => format!("export S6_LOGGING_SCRIPT=\"{s}\""),
        None => "".to_string(),
    };
    format!(
        indoc! {r#"
            #!/bin/sh
            {}
            exec logutil-service {}
        "#},
        script,
        path.display()
    )
}

// using trick here to log oneshots: https://github.com/just-containers/s6-overlay/issues/442
fn log_up(path: &Path, log_script: Option<String>, run_script: &Path) -> String {
    let log_script = match log_script {
        Some(s) => format!("export S6_LOGGING_SCRIPT \"{s}\""),
        None => "".to_string(),
    };
    format!(
        indoc! {r#"
            #!/command/execlineb -P
            {}
            pipeline -w {{ logutil-service {} }}
            fdmove -c 2 1
            {}
        "#},
        log_script,
        path.display(),
        run_script.display()
    )
}

fn no_restart_on_failure() -> String {
    format!(indoc! {r#"
        #!/bin/sh
        exit 125
    "#})
}
