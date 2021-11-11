use crate::build::{self, CargoWrapper};
use crate::diag;
use crate::manifest::ManifestProperties;

use std::collections::BTreeMap;
use std::fmt;
use std::fs;
use std::io::Write;

use cargo_metadata::Artifact;
use chrono::Local;
use include_dir::{dir::ExtractMode, include_dir};
use itm_decode::{ExceptionAction, MemoryAccessType, TimestampedTracePackets, TracePacket};

use proc_macro2::{Ident, TokenStream, TokenTree};
use quote::{format_ident, quote};
use rtic_scope_api::{self as api, EventChunk, EventType, TaskAction};

use serde::{Deserialize, Serialize};

use thiserror::Error;

type HwExceptionNumber = u16;
type SwExceptionNumber = usize;
type ExceptionIdent = String;
type TaskIdent = [String; 2];
type ExternalHwAssocs = BTreeMap<HwExceptionNumber, (TaskIdent, ExceptionIdent)>;
type InternalHwAssocs = BTreeMap<ExceptionIdent, TaskIdent>;
type SwAssocs = BTreeMap<SwExceptionNumber, Vec<String>>;

#[derive(Debug, Error)]
pub enum RecoveryError {
    #[error("The IRQ ({0:?}) -> RTIC task mapping does not exist")]
    MissingHWLabelExceptionMap(itm_decode::cortex_m::Exception),
    #[error("The IRQ ({0}) -> RTIC task mapping does not exist")]
    MissingHWExceptionMap(HwExceptionNumber),
    #[error("The DataTraceValue ({0:?}) -> RTIC task mapping does not exist")]
    MissingSWMap(Vec<u8>),
    #[error("Failed to read artifact source file: {0}")]
    SourceRead(#[source] std::io::Error),
    #[error("Failed to tokenize artifact source file: {0}")]
    TokenizeFail(#[source] syn::Error),
    #[error("Failed to find arguments to RTIC application")]
    RTICArgumentsMissing,
    #[error("Failed to parse the content of the RTIC application")]
    RTICParseFail(#[source] syn::Error),
    #[error("Failed to extract and/or configure the intermediate crate directory to disk: {0}")]
    LibExtractFail(#[source] std::io::Error),
    #[error("Failed to build the intermediate crate: {0}")]
    LibBuildFail(#[from] build::CargoError),
    #[error("Failed to load the intermediate shared object: {0}")]
    LibLoadFail(#[source] libloading::Error),
    #[error("Failed to lookup symbol in the intermediate shared object: {0}")]
    LibLookupFail(#[source] libloading::Error),
}

impl diag::DiagnosableError for RecoveryError {
    fn diagnose(&self) -> Vec<String> {
        match self {
            RecoveryError::RTICArgumentsMissing => vec![
                "RTIC Scope expects an RTIC application declaration on the form `#[app(...)] mod app { ... }` where the first `...` is the application arguments.".to_string(),
                "Change #[rtic::app(...)] to #[app(...)] via `use rtic::app;`.".to_string(),
            ],
            _ => vec![],
        }
    }
}

#[derive(Clone, Serialize, Deserialize)]
pub struct Metadata {
    program_name: String,
    maps: TaskResolveMaps,
    manip: ManifestProperties,
    timestamp: chrono::DateTime<Local>,
    freq: u32,
    comment: Option<String>,
}

impl Metadata {
    pub fn new(
        program_name: String,
        maps: TaskResolveMaps,
        manip: ManifestProperties,
        timestamp: chrono::DateTime<Local>,
        freq: u32,
        comment: Option<String>,
    ) -> Self {
        Self {
            program_name,
            maps,
            manip,
            timestamp,
            freq,
            comment,
        }
    }

    pub fn hardware_tasks(&self) -> usize {
        self.maps.exceptions.len() + self.maps.interrupts.len()
    }

    pub fn software_tasks(&self) -> usize {
        self.maps.sw_assocs.len()
    }

    pub fn comment(&self) -> String {
        self.comment.clone().unwrap_or("".to_string())
    }

    pub fn program_name(&self) -> String {
        self.program_name.clone()
    }

    pub fn build_event_chunk(&self, packets: TimestampedTracePackets) -> EventChunk {
        let timestamp = {
            let itm_decode::Timestamp {
                base,
                delta,
                data_relation,
                diverged,
            } = packets.timestamp;
            let seconds_since = (base.unwrap_or(0) + delta.expect("timestamp delta is None"))
                as f64
                / self.freq as f64;
            let since = chrono::Duration::nanoseconds((seconds_since * 1e9).round() as i64);

            api::Timestamp {
                ts: self.timestamp + since,
                data_relation,
                diverged,
            }
        };

        let resolve_hw_task = |&excpt| -> Result<String, RecoveryError> {
            use itm_decode::cortex_m::VectActive;

            match excpt {
                VectActive::ThreadMode => Ok("ThreadMode".to_string()),
                VectActive::Exception(e) => Ok(self
                    .maps
                    .exceptions
                    .get(&format!("{:?}", e))
                    .ok_or(RecoveryError::MissingHWLabelExceptionMap(e))?
                    .join("::")),
                VectActive::Interrupt { irqn } => {
                    let (fun, _bind) = self
                        .maps
                        .interrupts
                        .get(&irqn.into())
                        .ok_or(RecoveryError::MissingHWExceptionMap(irqn.into()))?;
                    Ok(fun.join("::"))
                }
            }
        };

        let resolve_sw_task = |value: Vec<u8>| -> Result<String, RecoveryError> {
            if value.iter().filter(|&b| *b > 0).count() > 1 || value.is_empty() {
                return Err(RecoveryError::MissingSWMap(value));
            }
            self.maps
                .sw_assocs
                .get(&(value[0] as usize))
                .map(|v| v.join("::"))
                .ok_or(RecoveryError::MissingSWMap(value))
        };

        // convert itm_decode::TracePacket -> api::EventType
        let mut events = vec![];
        for packet in packets.packets.iter() {
            match packet {
                TracePacket::Sync => (), // noop: only used for byte alignment; contains no data
                TracePacket::Overflow => {
                    events.push(EventType::Overflow);
                }
                TracePacket::ExceptionTrace { exception, action } => events.push(EventType::Task {
                    name: match resolve_hw_task(exception) {
                        Ok(name) => name,
                        Err(e) => {
                            events.push(EventType::Unmappable(packet.clone(), e.to_string()));
                            continue;
                        }
                    },
                    action: match action {
                        ExceptionAction::Entered => TaskAction::Entered,
                        ExceptionAction::Exited => TaskAction::Exited,
                        ExceptionAction::Returned => TaskAction::Returned,
                    },
                }),
                TracePacket::DataTraceValue {
                    comparator,
                    access_type,
                    value,
                } if *access_type == MemoryAccessType::Write => events.push(EventType::Task {
                    action: match *comparator as usize {
                        c if c == self.manip.dwt_enter_id => TaskAction::Entered,
                        c if c == self.manip.dwt_exit_id => TaskAction::Exited,
                        _ => {
                            events.push(EventType::Unknown(packet.clone()));
                            continue;
                        }
                    },
                    name: match resolve_sw_task(value.clone()) {
                        Ok(name) => name,
                        Err(e) => {
                            events.push(EventType::Unmappable(packet.clone(), e.to_string()));
                            continue;
                        }
                    },
                }),
                _ => events.push(EventType::Unknown(packet.clone())),
            }
        }

        // map malformed packets
        events.append(
            &mut packets
                .malformed_packets
                .iter()
                .map(|m| EventType::Invalid(m.to_owned()))
                .collect(),
        );

        EventChunk { timestamp, events }
    }
}

impl fmt::Display for Metadata {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(f, "{}", self.maps)?;
        writeln!(f, "reset timestamp: {}", self.timestamp)?;
        writeln!(f, "trace clock frequency: {} Hz", self.freq)?;

        Ok(())
    }
}

#[derive(Serialize, Deserialize, Clone)]
pub struct TaskResolveMaps {
    pub exceptions: InternalHwAssocs,
    pub interrupts: ExternalHwAssocs,
    pub sw_assocs: SwAssocs,
}

impl fmt::Display for TaskResolveMaps {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Here C++ reigns superior with its generic lambdas.
        macro_rules! display_map {
            ($h:expr, $m:expr) => {{
                writeln!(f, "{}:", $h)?;
                for (k, v) in $m.iter() {
                    writeln!(f, "        {} -> {:?}", k, v)?;
                }

                Ok(())
            }};
        }

        display_map!("exceptions", self.exceptions)?;
        display_map!("interrupts", self.interrupts)?;
        display_map!("software tasks", self.sw_assocs)
    }
}

pub struct TaskResolver<'a> {
    cargo: &'a CargoWrapper,
    app: TokenStream,
    app_args: TokenStream,
    pacp: ManifestProperties,
}

impl<'a> TaskResolver<'a> {
    pub fn new(
        artifact: &Artifact,
        cargo: &'a CargoWrapper,
        pacp: ManifestProperties,
    ) -> Result<Self, RecoveryError> {
        // parse the RTIC app from the source file
        let src =
            fs::read_to_string(&artifact.target.src_path).map_err(RecoveryError::SourceRead)?;
        let mut rtic_app = syn::parse_str::<TokenStream>(&src)
            .map_err(RecoveryError::TokenizeFail)?
            .into_iter()
            .skip_while(|token| {
                if let TokenTree::Group(g) = token {
                    if let Some(c) = g.stream().into_iter().next() {
                        return c.to_string().as_str() != "app";
                    }
                }
                true
            });
        let app_args = {
            let mut args: Option<TokenStream> = None;
            if let Some(TokenTree::Group(g)) = rtic_app.next() {
                if let Some(TokenTree::Group(g)) = g.stream().into_iter().nth(1) {
                    args = Some(g.stream());
                }
            }
            args.ok_or(RecoveryError::RTICArgumentsMissing)?
        };
        let app = rtic_app.collect::<TokenStream>();

        Ok(TaskResolver {
            cargo,
            app,
            app_args,
            pacp,
        })
    }

    pub fn resolve(&self) -> Result<TaskResolveMaps, RecoveryError> {
        let (exceptions, interrupts) = self.hardware_tasks()?;
        let sw_assocs = self.software_tasks();

        Ok(TaskResolveMaps {
            exceptions,
            interrupts,
            sw_assocs,
        })
    }

    /// Parses an RTIC `mod app { ... }` declaration and associates the full
    /// path of the functions that are decorated with the `#[trace]`-macro
    /// with it's assigned task ID.
    fn software_tasks(&self) -> SwAssocs {
        struct TaskIDGenerator(usize);
        impl TaskIDGenerator {
            pub fn new() -> Self {
                TaskIDGenerator(0)
            }

            /// Generate a unique task id. Returned values mirror the behavior
            /// of the `trace`-macro from the tracing module.
            pub fn generate(&mut self) -> usize {
                let id = self.0;
                self.0 += 1;
                id
            }
        }

        // NOTE(unwrap) the whole source file is parsed in [TaskResolver::new]
        let app = syn::parse2::<syn::Item>(self.app.clone()).unwrap();
        let mut ctx: Vec<syn::Ident> = vec![];
        let mut assocs = SwAssocs::new();
        let mut id_gen = TaskIDGenerator::new();

        fn traverse_item(
            item: &syn::Item,
            ctx: &mut Vec<syn::Ident>,
            assocs: &mut SwAssocs,
            id_gen: &mut TaskIDGenerator,
        ) {
            match item {
                // handle
                //
                //   #[trace]
                //   fn fun() {
                //       #[trace]
                //       fn sub_fun() {
                //           // ...
                //       }
                //   }
                //
                syn::Item::Fn(fun) => {
                    // record the full path of the function
                    ctx.push(fun.sig.ident.clone());

                    // is the function decorated with #[trace]?
                    if fun.attrs.iter().any(|a| a.path == syn::parse_quote!(trace)) {
                        assocs.insert(
                            id_gen.generate(),
                            ctx.iter().map(|i| i.to_string()).collect(),
                        );
                    }

                    // walk down all other nested functions
                    for item in fun.block.stmts.iter().filter_map(|stmt| match stmt {
                        syn::Stmt::Item(item) => Some(item),
                        _ => None,
                    }) {
                        traverse_item(item, ctx, assocs, id_gen);
                    }

                    // we've handled with function, return to upper scope
                    ctx.pop();
                }
                // handle
                //
                //   mod scope {
                //       #[trace]
                //       fn fun() {
                //           // ...
                //       }
                //   }
                //
                syn::Item::Mod(m) => {
                    ctx.push(m.ident.clone());
                    if let Some((_, items)) = &m.content {
                        for item in items {
                            traverse_item(item, ctx, assocs, id_gen);
                        }
                    }
                    ctx.pop();
                }
                _ => (),
            }
        }

        traverse_item(&app, &mut ctx, &mut assocs, &mut id_gen);

        assocs
    }

    /// Parses an RTIC `#[app(device = ...)] mod app { ... }` declaration
    /// and associates the full path of hardware task functions to their
    /// exception numbers as reported by the target.
    fn hardware_tasks(&self) -> Result<(InternalHwAssocs, ExternalHwAssocs), RecoveryError> {
        let (app, _analysis) = {
            let mut settings = rtic_syntax::Settings::default();
            settings.parse_binds = true;
            rtic_syntax::parse2(self.app_args.clone(), self.app.clone(), settings)
                .map_err(RecoveryError::RTICParseFail)?
        };

        // Find the bound exceptions from the #[task(bound = ...)]
        // arguments. Further, partition internal and external
        // interrupts. List of already known exceptions is extracted
        // from the ARMv7-M arch. reference manual, table B1-4.
        //
        // For external exceptions (those defined in PAC::Interrupt), we
        // need to resolve the number we receive over ITM back to the
        // interrupt name. For internal interrupts, the name of the
        // exception is received over ITM.
        let known_exceptions = [
            "Reset",
            "NMI",
            "HardFault",
            "MemManage",
            "BusFault",
            "UsageFault",
            "SVCall",
            "DebugMonitor",
            "PendSV",
            "SysTick",
        ];
        let (int_binds, ext_binds): (Vec<Ident>, Vec<Ident>) = app
            .hardware_tasks
            .iter()
            .map(|(_name, hwt)| hwt.args.binds.clone())
            .partition(|bind| known_exceptions.iter().any(|&int| *bind == int));
        let binds = ext_binds.clone();

        // Resolve exception numbers from bound idents
        let excpt_nrs = if ext_binds.is_empty() {
            BTreeMap::<Ident, HwExceptionNumber>::new()
        } else {
            self.resolve_int_nrs(&binds)?
        };

        let int_assocs: InternalHwAssocs = app
            .hardware_tasks
            .iter()
            .filter_map(|(name, hwt)| {
                let bind = &hwt.args.binds;
                if int_binds.iter().any(|b| b == bind) {
                    Some((bind.to_string(), ["app".to_string(), name.to_string()]))
                } else {
                    None
                }
            })
            .collect();

        let ext_assocs: ExternalHwAssocs = app
            .hardware_tasks
            .iter()
            .filter_map(|(name, hwt)| {
                let bind = &hwt.args.binds;
                excpt_nrs.get(bind).map(|int| {
                    (
                        *int,
                        (["app".to_string(), name.to_string()], bind.to_string()),
                    )
                })
            })
            .collect();

        Ok((int_assocs, ext_assocs))
    }

    fn resolve_int_nrs(
        &self,
        binds: &[Ident],
    ) -> Result<BTreeMap<Ident, HwExceptionNumber>, RecoveryError> {
        const ADHOC_FUNC_PREFIX: &str = "rtic_scope_func_";

        // Extract adhoc source to a temporary directory and apply adhoc
        // modifications.
        let target_dir = self.cargo.target_dir().join("cargo-rtic-trace-libadhoc");
        include_dir!("assets/libadhoc")
            .extract(&target_dir, ExtractMode::Overwrite)
            .map_err(RecoveryError::LibExtractFail)?;
        // NOTE See <https://github.com/rust-lang/cargo/issues/9643>
        fs::rename(
            target_dir.join("not-Cargo.toml"),
            target_dir.join("Cargo.toml"),
        )
        .map_err(RecoveryError::LibExtractFail)?;
        // Add required crate (and optional feature) as dependency
        {
            let mut manifest = fs::OpenOptions::new()
                .append(true)
                .open(target_dir.join("Cargo.toml"))
                .map_err(RecoveryError::LibExtractFail)?;
            let dep = format!(
                "\n{} = {{ version = \"{}\", features = [{}]}}\n",
                self.pacp.pac_name,
                self.pacp.pac_version,
                self.pacp
                    .pac_features
                    .iter()
                    .map(|f| format!("\"{}\"", f))
                    .collect::<Vec<String>>()
                    .join(","),
            );
            manifest
                .write_all(dep.as_bytes())
                .map_err(RecoveryError::LibExtractFail)?;
        }
        // Prepare lib.rs
        {
            // Import PAC::Interrupt
            let mut src = fs::OpenOptions::new()
                .append(true)
                .open(target_dir.join("src/lib.rs"))
                .map_err(RecoveryError::LibExtractFail)?;
            let import = str::parse::<TokenStream>(&self.pacp.interrupt_path)
                .expect("Failed to tokenize pacp.interrupt_path");
            let import = quote!(use #import;);
            src.write_all(format!("\n{}\n", import).as_bytes())
                .map_err(RecoveryError::LibExtractFail)?;

            // Generate the functions that must be exported
            for bind in binds {
                let fun = format_ident!("{}{}", ADHOC_FUNC_PREFIX, bind);
                let int_ident = format_ident!("{}", bind);
                let fun = quote!(
                    #[no_mangle]
                    pub extern fn #fun() -> u16 {
                        Interrupt::#int_ident.number()
                    }
                );
                src.write_all(format!("\n{}\n", fun).as_bytes())
                    .map_err(RecoveryError::LibExtractFail)?;
            }
        }

        // Build the adhoc library, load it, and resolve all exception idents
        let artifact = self.cargo.build(
            &target_dir,
            // Host target triple need not be specified when CARGO is set.
            None,
            "cdylib",
        )?;
        let lib = unsafe {
            libloading::Library::new(artifact.filenames.first().unwrap())
                .map_err(RecoveryError::LibLoadFail)?
        };
        let binds: Result<Vec<(proc_macro2::Ident, HwExceptionNumber)>, RecoveryError> = binds
            .iter()
            .map(|b| {
                let func: libloading::Symbol<extern "C" fn() -> HwExceptionNumber> = unsafe {
                    lib.get(format!("{}{}", ADHOC_FUNC_PREFIX, b).as_bytes())
                        .map_err(RecoveryError::LibLookupFail)?
                };
                Ok((b.clone(), func()))
            })
            .collect();
        Ok(binds?.iter().cloned().collect())
    }
}
