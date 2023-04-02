//! FIXME

// FIXME handle debuginfo

use std::collections::BTreeMap;
use std::sync::Arc;

use cranelift_codegen::control::ControlPlane;
use cranelift_codegen::entity::SecondaryMap;
use cranelift_codegen::ir::{self, UserExternalName};
use cranelift_module::{ModuleDeclarations, ModuleError, ModuleExtName, ModuleResult};
use cranelift_object::{ObjectBuilder, ObjectModule};
use object::{Object, ObjectSection};

use crate::prelude::*;

pub(super) struct SerializeModule {
    pub(super) isa: Arc<dyn isa::TargetIsa>,
    pub(super) inner: SerializeModuleInner,
}

#[derive(Debug, serde::Serialize, serde::Deserialize)]
pub(super) struct SerializeModuleInner {
    pub(super) declarations: cranelift_module::ModuleDeclarations,
    pub(super) functions: BTreeMap<FuncId, Function>,
    pub(super) data_objects: BTreeMap<DataId, DataDescription>,
}

#[cfg(feature = "lto")]
impl SerializeModule {
    fn serialize(self) -> Vec<u8> {
        bincode::serialize(&self.inner).unwrap()
    }

    pub(super) fn deserialize(blob: &[u8], isa: Arc<dyn isa::TargetIsa>) -> SerializeModule {
        SerializeModule { isa, inner: bincode::deserialize(blob).unwrap() }
    }

    pub(super) fn apply_to(self, module: &mut dyn Module) {
        //println!("{:#?}", self.inner);

        let mut function_map: SecondaryMap<FuncId, Option<FuncId>> = SecondaryMap::new();
        let mut data_object_map: SecondaryMap<DataId, Option<DataId>> = SecondaryMap::new();

        let mut remap_func_id =
            |module: &mut dyn Module, declarations: &ModuleDeclarations, func_id: FuncId| {
                if function_map[func_id].is_none() {
                    let decl = declarations.get_function_decl(func_id);
                    function_map[func_id] = Some(if let Some(name) = &decl.name {
                        module.declare_function(name, decl.linkage, &decl.signature).unwrap()
                    } else {
                        module.declare_anonymous_function(&decl.signature).unwrap()
                    });
                }
                function_map[func_id].unwrap()
            };

        let mut remap_data_id =
            |module: &mut dyn Module, declarations: &ModuleDeclarations, data_id: DataId| {
                if data_object_map[data_id].is_none() {
                    let decl = declarations.get_data_decl(data_id);
                    data_object_map[data_id] = Some(if let Some(name) = &decl.name {
                        module.declare_data(name, decl.linkage, decl.writable, decl.tls).unwrap()
                    } else {
                        module.declare_anonymous_data(decl.writable, decl.tls).unwrap()
                    });
                }
                data_object_map[data_id].unwrap()
            };

        for (func_id, func) in self.inner.functions {
            let func_id = remap_func_id(module, &self.inner.declarations, func_id);
            let mut ctx = Context::for_function(func);
            for (_, ext_name) in &mut ctx.func.params.user_named_funcs {
                if ext_name.namespace == 0 {
                    *ext_name = UserExternalName::new(
                        0,
                        remap_func_id(
                            module,
                            &self.inner.declarations,
                            FuncId::from_u32(ext_name.index),
                        )
                        .as_u32(),
                    );
                } else if ext_name.namespace == 1 {
                    *ext_name = UserExternalName::new(
                        1,
                        remap_data_id(
                            module,
                            &self.inner.declarations,
                            DataId::from_u32(ext_name.index),
                        )
                        .as_u32(),
                    );
                } else {
                    unreachable!();
                }
            }
            // FIXME remap user_ext_name_to_ref
            module.define_function(func_id, &mut ctx).unwrap();
        }

        for (data_id, mut data) in self.inner.data_objects {
            let data_id = remap_data_id(module, &self.inner.declarations, data_id);
            for ext_name in data
                .function_decls
                .iter_mut()
                .map(|(_, ext_name)| ext_name)
                .chain(data.data_decls.iter_mut().map(|(_, ext_name)| ext_name))
            {
                match *ext_name {
                    ModuleExtName::User { namespace, ref mut index } => {
                        if namespace == 0 {
                            *index = remap_func_id(
                                module,
                                &self.inner.declarations,
                                FuncId::from_u32(*index),
                            )
                            .as_u32();
                        } else if namespace == 1 {
                            *index = remap_data_id(
                                module,
                                &self.inner.declarations,
                                DataId::from_u32(*index),
                            )
                            .as_u32();
                        } else {
                            unreachable!();
                        }
                    }
                    ModuleExtName::KnownSymbol(_) | ModuleExtName::LibCall(_) => {}
                }
            }
            module.define_data(data_id, &data).unwrap();
        }

        //todo!();
    }
}

impl Module for SerializeModule {
    fn isa(&self) -> &dyn isa::TargetIsa {
        &*self.isa
    }

    fn declarations(&self) -> &cranelift_module::ModuleDeclarations {
        &self.inner.declarations
    }

    fn declare_function(
        &mut self,
        name: &str,
        linkage: Linkage,
        signature: &ir::Signature,
    ) -> ModuleResult<FuncId> {
        let (id, _linkage) = self.inner.declarations.declare_function(name, linkage, signature)?;

        Ok(id)
    }

    fn declare_anonymous_function(&mut self, signature: &ir::Signature) -> ModuleResult<FuncId> {
        Ok(self.inner.declarations.declare_anonymous_function(signature)?)
    }

    fn declare_data(
        &mut self,
        name: &str,
        linkage: Linkage,
        writable: bool,
        tls: bool,
    ) -> ModuleResult<DataId> {
        let (id, _linkage) = self.inner.declarations.declare_data(name, linkage, writable, tls)?;

        Ok(id)
    }

    fn declare_anonymous_data(&mut self, writable: bool, tls: bool) -> ModuleResult<DataId> {
        Ok(self.inner.declarations.declare_anonymous_data(writable, tls)?)
    }

    fn define_function_with_control_plane(
        &mut self,
        func_id: FuncId,
        ctx: &mut Context,
        _ctrl_plane: &mut ControlPlane,
    ) -> ModuleResult<()> {
        let decl = self.inner.declarations.get_function_decl(func_id);
        if !decl.linkage.is_definable() {
            return Err(ModuleError::InvalidImportDefinition(
                decl.name.as_deref().unwrap_or("<anonymous>").to_owned(),
            ));
        }

        if self.inner.functions.get(&func_id).is_some() {
            // FIXME handle multiple modules defining private functions with the same name
            return Ok(());
            return Err(ModuleError::DuplicateDefinition(
                decl.name.as_deref().unwrap_or("<anonymous>").to_owned(),
            ));
        }

        ctx.verify_if(&*self.isa)?;
        ctx.optimize(&*self.isa)?;

        self.inner.functions.insert(func_id, ctx.func.clone());

        Ok(())
    }

    fn define_function_bytes(
        &mut self,
        _func_id: FuncId,
        _func: &Function,
        _alignment: u64,
        _bytes: &[u8],
        _relocs: &[cranelift_codegen::MachReloc],
    ) -> ModuleResult<()> {
        unimplemented!()
    }

    fn define_data(&mut self, data_id: DataId, data: &DataDescription) -> ModuleResult<()> {
        let decl = self.inner.declarations.get_data_decl(data_id);
        if !decl.linkage.is_definable() {
            return Err(ModuleError::InvalidImportDefinition(
                decl.name.as_deref().unwrap_or("<anonymous>").to_owned(),
            ));
        }

        if self.inner.data_objects.get(&data_id).is_some() {
            return Err(ModuleError::DuplicateDefinition(
                decl.name.as_deref().unwrap_or("<anonymous>").to_owned(),
            ));
        }

        self.inner.data_objects.insert(data_id, data.clone());

        Ok(())
    }
}

use std::fs::File;

use rustc_codegen_ssa::back::metadata::create_compressed_metadata_file;
use rustc_codegen_ssa::{CodegenResults, CompiledModule, CrateInfo, ModuleKind};
use rustc_metadata::EncodedMetadata;
use rustc_middle::dep_graph::{WorkProduct, WorkProductId};
use rustc_middle::mir::mono::MonoItem;
use rustc_session::config::{DebugInfo, OutputType};
use rustc_session::Session;

use crate::BackendConfig;

pub(crate) struct OngoingCodegen {
    modules: Vec<CompiledModule>,
    allocator_module: Option<CompiledModule>,
    metadata_module: Option<CompiledModule>,
    metadata: EncodedMetadata,
    crate_info: CrateInfo,
}

impl OngoingCodegen {
    pub(crate) fn join(
        self,
        sess: &Session,
    ) -> (CodegenResults, FxHashMap<WorkProductId, WorkProduct>) {
        let mut modules = vec![];

        for module in self.modules {
            modules.push(module);
        }

        sess.abort_if_errors();

        (
            CodegenResults {
                modules,
                allocator_module: self.allocator_module,
                metadata_module: self.metadata_module,
                metadata: self.metadata,
                crate_info: self.crate_info,
            },
            FxHashMap::default(),
        )
    }
}

pub(super) fn make_module(sess: &Session, backend_config: &BackendConfig) -> SerializeModule {
    let isa = crate::build_isa(sess, backend_config);

    SerializeModule {
        isa,
        inner: SerializeModuleInner {
            declarations: ModuleDeclarations::default(),
            functions: BTreeMap::new(),
            data_objects: BTreeMap::new(),
        },
    }
}

fn emit_module(
    tcx: TyCtxt<'_>,
    backend_config: &BackendConfig,
    serialize_module: SerializeModule,
    kind: ModuleKind,
    name: String,
) -> Result<CompiledModule, String> {
    if !(tcx.sess.crate_types().len() == 1
        && tcx.sess.crate_types()[0] == rustc_session::config::CrateType::Rlib)
    {
        let isa = crate::build_isa(tcx.sess, backend_config);

        let mut builder =
            ObjectBuilder::new(isa, name.clone() + ".o", cranelift_module::default_libcall_names())
                .unwrap();
        // Unlike cg_llvm, cg_clif defaults to disabling -Zfunction-sections. For cg_llvm binary size
        // is important, while cg_clif cares more about compilation times. Enabling -Zfunction-sections
        // can easily double the amount of time necessary to perform linking.
        builder
            .per_function_section(tcx.sess.opts.unstable_opts.function_sections.unwrap_or(false));
        let aot_module = ObjectModule::new(builder);
        let unwind_context = UnwindContext::new(aot_module.isa(), true);
        let mut module = super::aot::AOTModule { aot_module, unwind_context };

        serialize_module.apply_to(&mut module);

        let mut product = module.finish();

        if product.object.format() == cranelift_object::object::BinaryFormat::Elf {
            let comment_section = product.object.add_section(
                Vec::new(),
                b".comment".to_vec(),
                cranelift_object::object::SectionKind::OtherString,
            );
            let mut producer = vec![0];
            producer.extend(crate::debuginfo::producer().as_bytes());
            producer.push(0);
            product.object.set_section_data(comment_section, producer, 1);
        }

        let tmp_file = tcx.output_filenames(()).temp_path(OutputType::Object, Some(&name));
        let mut file = match File::create(&tmp_file) {
            Ok(file) => file,
            Err(err) => return Err(format!("error creating object file: {}", err)),
        };

        if let Err(err) = product.object.write_stream(&mut file) {
            return Err(format!("error writing object file: {}", err));
        }

        tcx.prof.artifact_size("object_file", &*name, file.metadata().unwrap().len());

        Ok(CompiledModule {
            name,
            kind,
            object: Some(tmp_file),
            dwarf_object: None,
            bytecode: None,
        })
    } else {
        let mut object = object::write::Object::new(
            object::BinaryFormat::Elf,
            object::Architecture::Aarch64,
            object::Endianness::Little,
        );
        object.add_subsection(
            object::write::StandardSection::ReadOnlyData,
            b"cgclif_lto",
            &serialize_module.serialize(),
            1,
        );

        let bytecode_file = tcx.output_filenames(()).temp_path(OutputType::Object, Some(&name));
        let mut file = match File::create(&bytecode_file) {
            Ok(file) => file,
            Err(err) => return Err(format!("error creating object file: {}", err)),
        };

        if let Err(err) = object.write_stream(&mut file) {
            return Err(format!("error writing object file: {}", err));
        }

        tcx.prof.artifact_size("object_file", &*name, file.metadata().unwrap().len());

        Ok(CompiledModule {
            name,
            kind,
            object: Some(bytecode_file),
            dwarf_object: None,
            bytecode: None,
        })
    }
}

fn module_codegen(
    tcx: TyCtxt<'_>,
    (backend_config, cgu_name): (BackendConfig, rustc_span::Symbol),
) -> CompiledModule {
    let (cgu_name, mut cx, mut module, codegened_functions) =
        tcx.prof.verbose_generic_activity_with_arg("codegen cgu", cgu_name.as_str()).run(|| {
            let cgu = tcx.codegen_unit(cgu_name);
            let mono_items = cgu.items_in_deterministic_order(tcx);

            let mut module = make_module(tcx.sess, &backend_config);

            let mut cx = crate::CodegenCx::new(
                tcx,
                module.isa(),
                tcx.sess.opts.debuginfo != DebugInfo::None,
                cgu_name,
            );
            super::predefine_mono_items(tcx, &mut module, &mono_items);
            let mut codegened_functions = vec![];
            for (mono_item, _) in mono_items {
                match mono_item {
                    MonoItem::Fn(inst) => {
                        let codegened_function = crate::base::codegen_fn(
                            tcx,
                            &mut cx,
                            Function::new(),
                            &mut module,
                            inst,
                        );
                        codegened_functions.push(codegened_function);
                    }
                    MonoItem::Static(def_id) => {
                        crate::constant::codegen_static(tcx, &mut module, def_id)
                    }
                    MonoItem::GlobalAsm(item_id) => {
                        let item = tcx.hir().item(item_id);
                        tcx.sess
                            .span_fatal(item.span, "Global asm is not yet supported in LTO mode");
                    }
                }
            }
            crate::main_shim::maybe_create_entry_wrapper(tcx, &mut module, false, cgu.is_primary());

            let cgu_name = cgu.name().as_str().to_owned();

            (cgu_name, cx, module, codegened_functions)
        });

    match (move || {
        cx.profiler.clone().verbose_generic_activity_with_arg("compile functions", &*cgu_name).run(
            || {
                let mut cached_context = Context::new();
                for codegened_func in codegened_functions {
                    crate::base::compile_fn(
                        &mut cx,
                        &mut cached_context,
                        &mut module,
                        codegened_func,
                    );
                }
            },
        );

        // FIXME handle inline asm
        if !cx.global_asm.is_empty() {
            tcx.sess.fatal("Inline asm is not yet supported in LTO mode");
        }

        let codegen_result =
            cx.profiler.verbose_generic_activity_with_arg("write object file", &*cgu_name).run(
                || emit_module(tcx, &backend_config, module, ModuleKind::Regular, cgu_name.clone()),
            );
        codegen_result
    })() {
        Ok(res) => res,
        Err(err) => tcx.sess.fatal(err),
    }
}

pub(super) fn load_lto_modules(
    tcx: TyCtxt<'_>,
    crate_info: &CrateInfo,
    backend_config: &BackendConfig,
) -> Vec<(String, SerializeModule)> {
    let mut modules = vec![];

    if !(tcx.sess.crate_types().len() == 1
        && tcx.sess.crate_types()[0] == rustc_session::config::CrateType::Rlib)
    {
        let mut each_linked_rlib_for_lto = Vec::new();
        drop(rustc_codegen_ssa::back::link::each_linked_rlib(
            &crate_info,
            None,
            &mut |cnum, path| {
                if rustc_codegen_ssa::back::link::ignored_for_lto(tcx.sess, &crate_info, cnum) {
                    return;
                }
                each_linked_rlib_for_lto.push((cnum, path.to_path_buf()));
            },
        ));

        for (_cnum, path) in each_linked_rlib_for_lto {
            let archive_data = unsafe {
                rustc_data_structures::memmap::Mmap::map(
                    std::fs::File::open(&path).expect("couldn't open rlib"),
                )
                .expect("couldn't map rlib")
            };
            let archive =
                object::read::archive::ArchiveFile::parse(&*archive_data).expect("wanted an rlib");
            let obj_files = archive
                .members()
                .filter_map(|child| {
                    child.ok().and_then(|c| {
                        std::str::from_utf8(c.name()).ok().map(|name| (name.trim(), c))
                    })
                })
                .filter(|&(name, _)| rustc_codegen_ssa::looks_like_rust_object_file(name));
            for (name, child) in obj_files {
                let lto_object =
                    object::read::File::parse(child.data(&*archive_data).expect("corrupt rlib"))
                        .unwrap();
                let module = SerializeModule::deserialize(
                    lto_object.section_by_name(".rodata.cgclif_lto").unwrap().data().unwrap(),
                    crate::build_isa(tcx.sess, &backend_config),
                );
                modules.push((name.to_owned(), module));
            }
        }
    }

    modules
}

pub(crate) fn run_aot(
    tcx: TyCtxt<'_>,
    backend_config: BackendConfig,
    metadata: EncodedMetadata,
    need_metadata_module: bool,
) -> Box<OngoingCodegen> {
    // FIXME handle `-Ctarget-cpu=native`
    let target_cpu = match tcx.sess.opts.cg.target_cpu {
        Some(ref name) => name,
        None => tcx.sess.target.cpu.as_ref(),
    }
    .to_owned();

    let crate_info = CrateInfo::new(tcx, target_cpu);

    let cgus = if tcx.sess.opts.output_types.should_codegen() {
        tcx.collect_and_partition_mono_items(()).1
    } else {
        // If only `--emit metadata` is used, we shouldn't perform any codegen.
        // Also `tcx.collect_and_partition_mono_items` may panic in that case.
        &[]
    };

    let mut modules = tcx.sess.time("codegen mono items", || {
        cgus.iter()
            .map(|cgu| module_codegen(tcx, (backend_config.clone(), cgu.name())))
            .collect::<Vec<_>>()
    });

    for (name, module) in load_lto_modules(tcx, &crate_info, &backend_config) {
        modules.push(emit_module(tcx, &backend_config, module, ModuleKind::Regular, name).unwrap());
    }

    let mut allocator_module = make_module(tcx.sess, &backend_config);
    let created_alloc_shim = crate::allocator::codegen(tcx, &mut allocator_module);

    let allocator_module = if created_alloc_shim {
        match emit_module(
            tcx,
            &backend_config,
            allocator_module,
            ModuleKind::Allocator,
            "allocator_shim".to_owned(),
        ) {
            Ok(allocator_module) => Some(allocator_module),
            Err(err) => tcx.sess.fatal(err),
        }
    } else {
        None
    };

    let metadata_module = if need_metadata_module {
        let (metadata_cgu_name, tmp_file) = tcx.sess.time("write compressed metadata", || {
            use rustc_middle::mir::mono::CodegenUnitNameBuilder;

            let cgu_name_builder = &mut CodegenUnitNameBuilder::new(tcx);
            let metadata_cgu_name = cgu_name_builder
                .build_cgu_name(LOCAL_CRATE, &["crate"], Some("metadata"))
                .as_str()
                .to_string();

            let tmp_file =
                tcx.output_filenames(()).temp_path(OutputType::Metadata, Some(&metadata_cgu_name));

            let symbol_name = rustc_middle::middle::exported_symbols::metadata_symbol_name(tcx);
            let obj = create_compressed_metadata_file(tcx.sess, &metadata, &symbol_name);

            if let Err(err) = std::fs::write(&tmp_file, obj) {
                tcx.sess.fatal(&format!("error writing metadata object file: {}", err));
            }

            (metadata_cgu_name, tmp_file)
        });

        Some(CompiledModule {
            name: metadata_cgu_name,
            kind: ModuleKind::Metadata,
            object: Some(tmp_file),
            dwarf_object: None,
            bytecode: None,
        })
    } else {
        None
    };

    Box::new(OngoingCodegen { modules, allocator_module, metadata_module, metadata, crate_info })
}