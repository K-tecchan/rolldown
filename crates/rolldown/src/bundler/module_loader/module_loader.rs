use index_vec::IndexVec;
use rolldown_common::{ModuleId, RawPath, ResourceId};
use rolldown_resolver::Resolver;
use rolldown_utils::block_on_spawn_all;
use rustc_hash::FxHashMap;

use super::module_task::ModuleTask;
use super::task_result::TaskResult;
use super::Msg;
use crate::bundler::graph::graph::Graph;
use crate::bundler::graph::symbols::{SymbolMap, Symbols};
use crate::bundler::module::external_module::ExternalModule;
use crate::bundler::module::module::Module;
use crate::bundler::options::normalized_input_options::NormalizedInputOptions;
use crate::bundler::resolve_id::{resolve_id, ResolvedRequestInfo};
use crate::BuildError;
use crate::SharedResolver;

pub struct ModuleLoader<'a> {
  input_options: &'a NormalizedInputOptions,
  graph: &'a mut Graph,
  resolver: SharedResolver,
  visited: FxHashMap<RawPath, ModuleId>,
  remaining: u32,
  tx: tokio::sync::mpsc::UnboundedSender<Msg>,
  rx: tokio::sync::mpsc::UnboundedReceiver<Msg>,
}

impl<'a> ModuleLoader<'a> {
  pub fn new(input_options: &'a NormalizedInputOptions, graph: &'a mut Graph) -> Self {
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<Msg>();
    Self {
      tx,
      rx,
      input_options,
      resolver: Resolver::with_cwd(input_options.cwd.clone(), false).into(),
      visited: Default::default(),
      remaining: Default::default(),
      graph,
    }
  }

  pub async fn fetch_all_modules(&mut self) -> anyhow::Result<()> {
    if self.input_options.input.is_empty() {
      return Err(anyhow::format_err!(
        "You must supply options.input to rolldown"
      ));
    }

    let resolved_entries = self.resolve_entries().await?;

    let mut intermediate_modules: IndexVec<ModuleId, Option<Module>> =
      IndexVec::with_capacity(self.graph.entries.len());

    self.graph.entries = resolved_entries
      .into_iter()
      .map(|p| self.try_spawn_new_task(&p, &mut intermediate_modules))
      .collect();

    let mut tables: IndexVec<ModuleId, SymbolMap> = Default::default();

    while self.remaining > 0 {
      let Some(msg) = self.rx.recv().await else { break };
      match msg {
        Msg::Done(task_result) => {
          let TaskResult {
            module_id,
            symbol_map: symbol_table,
            resolved_deps,
            mut builder,
            ..
          } = task_result;

          let import_records = builder.import_records.as_mut().unwrap();

          resolved_deps
            .into_iter()
            .for_each(|(import_record_idx, info)| {
              let id = self.try_spawn_new_task(&info, &mut intermediate_modules);
              import_records[import_record_idx].resolved_module = id;
              while tables.len() <= id.raw() as usize {
                tables.push(Default::default());
              }
            });

          while tables.len() <= task_result.module_id.raw() as usize {
            tables.push(Default::default());
          }
          intermediate_modules[module_id] = Some(Module::Normal(builder.build()));

          tables[task_result.module_id] = symbol_table
        }
      }
      self.remaining -= 1;
    }
    self.graph.symbols = Symbols::new(tables);
    self.graph.modules = intermediate_modules
      .into_iter()
      .map(|m| m.unwrap())
      .collect();
    Ok(())
  }

  async fn resolve_entries(&mut self) -> anyhow::Result<Vec<ResolvedRequestInfo>> {
    let resolver = &self.resolver;

    let resolved_ids = block_on_spawn_all(self.input_options.input.iter().map(
      |input_item| async move {
        let specifier = &input_item.import;
        let resolve_id = resolve_id(resolver, specifier, None, false).await.unwrap();

        let Some(info) = resolve_id else {
          return Err(BuildError::unresolved_entry(specifier))
        };

        if info.is_external {
          return Err(BuildError::entry_cannot_be_external(info.path.as_str()));
        }

        Ok(info)
      },
    ));

    let mut errors = vec![];

    let ret = resolved_ids
      .into_iter()
      .filter_map(|handle| match handle {
        Ok(id) => Some(id),
        Err(e) => {
          errors.push(e);
          None
        }
      })
      .collect();

    Ok(ret)
  }

  fn try_spawn_new_task(
    &mut self,
    info: &ResolvedRequestInfo,
    intermediate_modules: &mut IndexVec<ModuleId, Option<Module>>,
  ) -> ModuleId {
    match self.visited.entry(info.path.clone()) {
      std::collections::hash_map::Entry::Occupied(visited) => *visited.get(),
      std::collections::hash_map::Entry::Vacant(not_visited) => {
        let id = intermediate_modules.push(None);
        if info.is_external {
          let ext = ExternalModule::new(
            id,
            ResourceId::new(info.path.clone(), &self.input_options.cwd),
          );
          intermediate_modules[id] = Some(Module::External(ext));
        } else {
          not_visited.insert(id);

          self.remaining += 1;

          let module_path = ResourceId::new(info.path.clone(), &self.input_options.cwd);

          let task = ModuleTask::new(id, self.resolver.clone(), module_path, self.tx.clone());
          tokio::spawn(async move { task.run().await });
        }
        id
      }
    }
  }
}