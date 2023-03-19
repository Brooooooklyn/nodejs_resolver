use crate::{
    description::PkgInfo,
    info::NormalizedPath,
    log::color,
    plugin::{
        BrowserFieldPlugin, ExportsFieldPlugin, ImportsFieldPlugin, MainFieldPlugin,
        MainFilePlugin, Plugin,
    },
    Context, EnforceExtension, Info, ResolveResult, Resolver, State,
};
use std::{
    borrow::Cow,
    path::{Path, PathBuf},
};

impl Resolver {
    pub(crate) fn append_ext_for_path(path: &Path, ext: &str) -> PathBuf {
        PathBuf::from(&format!("{}{ext}", path.display()))
    }

    fn resolve_file_with_ext(&self, path: PathBuf, info: Info) -> State {
        for ext in &self.options.extensions {
            let path = Self::append_ext_for_path(&path, ext);
            if self.load_entry(&path).is_file() {
                return State::Success(ResolveResult::Info(info.with_path(path).with_target("")));
            }
        }
        tracing::debug!(
            "'{}[{}]' is not a file",
            color::red(&path.display()),
            color::red(&self.options.extensions.join("|"))
        );
        State::Resolving(info)
    }

    pub(crate) fn resolve_as_context(&self, info: Info) -> State {
        if !self.options.resolve_to_context {
            return State::Resolving(info);
        }
        let path = info.to_resolved_path();
        tracing::debug!(
            "Attempting to load '{}' as a context",
            color::blue(&path.display())
        );
        if self.load_entry(&path).is_dir() {
            State::Success(ResolveResult::Info(Info::new(path, Default::default())))
        } else {
            State::Failed(info)
        }
    }

    #[tracing::instrument]
    pub(crate) fn resolve_as_file(&self, info: Info) -> State {
        if info.request().is_directory() {
            return State::Resolving(info);
        }
        let path = info.to_resolved_path();
        tracing::debug!(
            "Attempting to load '{}' as a file",
            color::blue(&path.display())
        );
        if matches!(self.options.enforce_extension, EnforceExtension::Enabled) {
            return self.resolve_file_with_ext(path.to_path_buf(), info);
        }
        if self.load_entry(&path).is_file() {
            let path = path.to_path_buf();
            State::Success(ResolveResult::Info(info.with_path(path).with_target("")))
        } else {
            self.resolve_file_with_ext(path.to_path_buf(), info)
        }
    }

    #[tracing::instrument]
    pub(crate) fn resolve_as_dir(&self, info: Info, context: &mut Context) -> State {
        let dir = info.to_resolved_path();
        let entry = self.load_entry(&dir);
        if !entry.is_dir() {
            return State::Failed(info);
        }
        let pkg_info = match entry.pkg_info(self) {
            Ok(pkg_info) => pkg_info,
            Err(err) => return State::Error(err),
        };
        let dir = dir.to_path_buf();
        let info = info.with_path(dir).with_target(".");
        if let Some(pkg_info) = pkg_info {
            MainFieldPlugin::new(pkg_info).apply(self, info, context)
        } else {
            State::Resolving(info)
        }
        .then(|info| MainFilePlugin.apply(self, info, context))
    }

    pub(crate) fn resolve_as_modules(&self, info: Info, context: &mut Context) -> State {
        let original_dir = info.normalized_path();
        for module in &self.options.modules {
            let node_modules_path = Path::new(module);
            let (node_modules_path, need_find_up) = if node_modules_path.is_absolute() {
                (Cow::Borrowed(node_modules_path), false)
            } else {
                (Cow::Owned(original_dir.as_ref().join(module)), true)
            };
            let state = self
                ._resolve_as_modules(info.clone(), original_dir, &node_modules_path, context)
                .then(|info| {
                    if !need_find_up {
                        State::Resolving(info)
                    } else if let Some(parent_dir) = original_dir.as_ref().parent() {
                        self._resolve(info.with_path(parent_dir), context)
                    } else {
                        State::Resolving(info)
                    }
                });
            if state.is_finished() {
                return state;
            }
        }
        State::Failed(info)
    }

    fn _resolve_as_modules(
        &self,
        info: Info,
        original_dir: &NormalizedPath,
        node_modules_path: &Path,
        context: &mut Context,
    ) -> State {
        let entry = self.load_entry(node_modules_path);
        let pkg_info = match entry.pkg_info(self) {
            Ok(pkg_info) => pkg_info.as_ref(),
            Err(err) => return State::Error(err),
        };
        let state = if entry.is_dir() {
            // is there had `node_modules` folder?
            self.resolve_node_modules(info, node_modules_path, context)
                .then(|info| {
                    let is_resolve_self = pkg_info.map_or(false, |pkg_info| {
                        let request_module_name =
                            get_module_name_from_request(info.request().target());
                        is_resolve_self(pkg_info, request_module_name)
                    });
                    if is_resolve_self {
                        let pkg_info = pkg_info.unwrap();
                        ExportsFieldPlugin::new(pkg_info).apply(self, info, context)
                    } else {
                        State::Resolving(info)
                    }
                })
        } else if pkg_info.map_or(false, |pkg_info| pkg_info.dir().eq(original_dir)) {
            // is `info.path` on the same level as package.json
            let request_module_name = get_module_name_from_request(info.request().target());
            if is_resolve_self(pkg_info.unwrap(), request_module_name) {
                ExportsFieldPlugin::new(pkg_info.unwrap()).apply(self, info, context)
            } else {
                State::Resolving(info)
            }
        } else {
            State::Resolving(info)
        };

        state
    }

    fn resolve_node_modules(
        &self,
        info: Info,
        node_modules_path: &Path,
        context: &mut Context,
    ) -> State {
        let original_dir = info.normalized_path();
        let request_module_name = get_module_name_from_request(info.request().target());
        let module_path = node_modules_path.join(request_module_name);
        let entry = self.load_entry(&module_path);
        let module_info = Info::new(node_modules_path, info.request().clone());
        if !entry.is_dir() {
            let state = self.resolve_as_file(module_info);
            if state.is_finished() {
                state
            } else {
                State::Resolving(info)
            }
        } else {
            let pkg_info = match entry.pkg_info(self) {
                Ok(pkg_info) => pkg_info,
                Err(err) => return State::Error(err),
            };
            let state = if let Some(pkg_info) = pkg_info {
                let out_node_modules = pkg_info.dir().eq(original_dir);
                if !out_node_modules || is_resolve_self(pkg_info, request_module_name) {
                    ExportsFieldPlugin::new(pkg_info).apply(self, module_info, context)
                } else {
                    State::Resolving(module_info)
                }
                .then(|info| ImportsFieldPlugin::new(pkg_info).apply(self, info, context))
                .then(|info| {
                    let path = info
                        .normalized_path()
                        .as_ref()
                        .join(info.request().target());
                    let info = info.with_path(path).with_target(".");
                    MainFieldPlugin::new(pkg_info).apply(self, info, context)
                })
                .then(|info| BrowserFieldPlugin::new(pkg_info).apply(self, info, context))
            } else {
                State::Resolving(module_info)
            }
            .then(|info| self.resolve_as_context(info))
            .then(|info| self.resolve_as_file(info))
            .then(|info| self.resolve_as_dir(info, context));

            match state {
                State::Failed(info) => State::Resolving(info),
                _ => state,
            }
        }
    }
}

fn is_resolve_self(pkg_info: &PkgInfo, request_module_name: &str) -> bool {
    pkg_info
        .json
        .name()
        .map(|pkg_name| request_module_name == pkg_name)
        .map_or(false, |ans| ans)
}

/// split the index from `[module-name]/[path]`
fn split_slash_from_request(target: &str) -> Option<usize> {
    let has_namespace_scope = target.starts_with('@');
    let chars = target.chars().enumerate();
    let slash_index_list: Vec<usize> = chars
        .filter_map(|(index, char)| if '/' == char { Some(index) } else { None })
        .collect();
    if has_namespace_scope {
        slash_index_list.get(1)
    } else {
        slash_index_list.first()
    }
    .copied()
}

fn get_module_name_from_request(target: &str) -> &str {
    split_slash_from_request(target).map_or(target, |index| &target[0..index])
}

pub(crate) fn get_path_from_request(target: &str) -> Option<Cow<str>> {
    split_slash_from_request(target).map(|index| Cow::Borrowed(&target[index..]))
}

#[cfg(test)]
mod test {
    use super::{get_module_name_from_request, get_path_from_request, split_slash_from_request};

    #[test]
    fn test_split_slash_from_request() {
        assert_eq!(split_slash_from_request("a"), None);
        assert_eq!(split_slash_from_request("a/b"), Some(1));
        assert_eq!(split_slash_from_request("@a"), None);
        assert_eq!(split_slash_from_request("@a/b"), None);
        assert_eq!(split_slash_from_request("@a/b/c"), Some(4));
    }

    #[test]
    fn test_get_module_name_from_request() {
        assert_eq!(get_module_name_from_request("a"), "a");
        assert_eq!(get_module_name_from_request("a/b"), "a");
        assert_eq!(get_module_name_from_request("@a"), "@a");
        assert_eq!(get_module_name_from_request("@a/b"), "@a/b");
        assert_eq!(get_module_name_from_request("@a/b/c"), "@a/b");
    }

    #[test]
    fn test_get_path_from_request() {
        assert_eq!(get_path_from_request("a"), None);
        assert_eq!(get_path_from_request("a/b"), Some("/b".into()));
        assert_eq!(get_path_from_request("@a"), None);
        assert_eq!(get_path_from_request("@a/b"), None);
        assert_eq!(get_path_from_request("@a/b/c"), Some("/c".into()));
    }
}
