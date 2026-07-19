use crate::{
    package,
    semver::{Constraint, Range, Version},
    solver::{incompat::Incompatibility, retriever, summary},
};
use anyhow::{anyhow, bail, Result};
use fs2::FileExt;
use serde::ser::Serialize;
use std::{
    collections::HashMap,
    env, fmt,
    fs::{self, DirBuilder, File, OpenOptions},
    io::{BufReader, BufWriter},
    path::PathBuf,
};
use tracing::{debug, info, warn};

pub struct Retriever {
    deps_cache: HashMap<Summary, Vec<Incompatibility<PackageId>>>,
    versions: HashMap<PackageId, Vec<Version>>,
    preferred_versions: HashMap<PackageId, Version>,
    mode: Mode,
    offline: bool,
}

type Summary = summary::Summary<PackageId>;

pub enum Mode {
    Minimize,
    Maximize,
}

#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub enum PackageId {
    Root,
    Elm,
    Pkg(package::Name),
}

impl summary::PackageId for PackageId {
    fn is_root(&self) -> bool {
        self == &PackageId::Root
    }
}

impl fmt::Display for PackageId {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            PackageId::Root => write!(f, "root"),
            PackageId::Elm => write!(f, "Elm"),
            PackageId::Pkg(name) => write!(f, "{}", name),
        }
    }
}

impl From<package::Name> for PackageId {
    fn from(n: package::Name) -> Self {
        PackageId::Pkg(n)
    }
}

impl PackageId {
    pub fn is(&self, n: &package::Name) -> bool {
        match self {
            PackageId::Root => false,
            PackageId::Elm => false,
            PackageId::Pkg(name) => name == n,
        }
    }
}

impl Retriever {
    pub async fn new(elm_version: &Constraint, offline: bool) -> Result<Self> {
        let mut deps_cache = HashMap::new();

        deps_cache.insert(
            Self::root(),
            vec![Incompatibility::from_dep(
                Self::root(),
                (PackageId::Elm, elm_version.complement()),
            )],
        );

        let mut retriever = Self {
            deps_cache,
            versions: HashMap::new(),
            preferred_versions: HashMap::new(),
            mode: Mode::Maximize,
            offline,
        };

        retriever.fetch_versions().await?;
        Ok(retriever)
    }

    pub fn minimize(&mut self) {
        self.mode = Mode::Minimize;
    }

    pub fn add_deps<'a, I>(&mut self, deps: I)
    where
        I: IntoIterator<Item = &'a (package::Name, Range)>,
    {
        let entry = self.deps_cache.entry(Self::root()).or_insert_with(Vec::new);
        entry.extend(deps.into_iter().map(|(name, range)| {
            let constraint = Constraint::from(range.clone()).complement();
            Incompatibility::from_dep(Self::root(), (name.clone().into(), constraint))
        }));
    }

    pub fn add_dep(&mut self, name: package::Name, version: Option<Constraint>) {
        let constraint = version.map_or_else(Constraint::empty, |x| x.complement());
        let deps = self.deps_cache.entry(Self::root()).or_insert_with(Vec::new);
        deps.push(Incompatibility::from_dep(
            Self::root(),
            (name.into(), constraint),
        ));
    }

    fn count_versions(versions_map: &HashMap<package::Name, Vec<Version>>) -> usize {
        let mut count = 0;
        for vs in versions_map.values() {
            count += vs.len();
        }
        count
    }

    async fn fetch_versions(&mut self) -> Result<()> {
        let file = Self::cache_file()?;
        file.lock_exclusive()?;

        let mut versions: HashMap<_, _> = self.fetch_cached_versions(&file).unwrap_or_default();

        if !self.offline {
            let count = Self::count_versions(&versions);

            let remote_versions = self.fetch_remote_versions(count).await.unwrap_or_else(|_| {
                warn!("Failed to fetch versions from package.elm-lang.org");
                HashMap::new()
            });

            let mut changed = false;

            for (pkg, vs) in &remote_versions {
                let entry = versions.entry(pkg.clone()).or_insert_with(Vec::new);
                entry.extend(vs);
                changed = true;
            }

            if changed {
                self.save_cached_versions(&file, &versions)?;
            }
        }

        file.unlock()?;

        // Merge in locally installed packages (e.g., Lamdera packages)
        let local_versions = self.fetch_local_versions().unwrap_or_else(|e| {
            debug!("Failed to scan local packages: {}", e);
            HashMap::new()
        });

        for (pkg, vs) in local_versions {
            let entry = versions.entry(pkg).or_insert_with(Vec::new);
            for v in vs {
                if !entry.contains(&v) {
                    entry.push(v);
                }
            }
        }

        let mut versions: HashMap<PackageId, Vec<Version>> = versions
            .iter()
            .map(|(k, v)| (k.clone().into(), v.clone()))
            .collect();

        versions.insert(PackageId::Root, vec![Version::new(1, 0, 0)]);
        versions.insert(
            PackageId::Elm,
            vec![
                Version::new(0, 14, 0),
                Version::new(0, 15, 0),
                Version::new(0, 16, 0),
                Version::new(0, 17, 0),
                Version::new(0, 18, 0),
                Version::new(0, 19, 0),
                Version::new(0, 19, 1),
            ],
        );

        self.versions = versions;
        Ok(())
    }

    fn fetch_cached_versions(
        &self,
        cache_file: &File,
    ) -> Result<HashMap<package::Name, Vec<Version>>> {
        let versions: HashMap<package::Name, Vec<Version>> = bincode::deserialize_from(cache_file)?;

        Ok(versions)
    }

    fn fetch_local_versions(&self) -> Result<HashMap<package::Name, Vec<Version>>> {
        let mut versions: HashMap<package::Name, Vec<Version>> = HashMap::new();
        let packages_path = Self::packages_path()?;

        // Scan both 0.19.0 and 0.19.1 packages directories
        for elm_version in &["0.19.0", "0.19.1"] {
            let mut path = packages_path.clone();
            path.push(elm_version);
            path.push("packages");

            if !path.exists() {
                continue;
            }

            // Iterate through author directories
            if let Ok(authors) = fs::read_dir(&path) {
                for author_entry in authors.flatten() {
                    let author_path = author_entry.path();
                    if !author_path.is_dir() {
                        continue;
                    }
                    let author_name = author_entry.file_name().to_string_lossy().to_string();

                    // Iterate through project directories
                    if let Ok(projects) = fs::read_dir(&author_path) {
                        for project_entry in projects.flatten() {
                            let project_path = project_entry.path();
                            if !project_path.is_dir() {
                                continue;
                            }
                            let project_name =
                                project_entry.file_name().to_string_lossy().to_string();

                            // Try to create a valid package name
                            let pkg_name = match package::Name::new(&author_name, &project_name) {
                                Ok(name) => name,
                                Err(_) => continue,
                            };

                            // Iterate through version directories
                            if let Ok(version_dirs) = fs::read_dir(&project_path) {
                                for version_entry in version_dirs.flatten() {
                                    let version_path = version_entry.path();
                                    if !version_path.is_dir() {
                                        continue;
                                    }

                                    // Check if elm.json exists in this version directory
                                    let elm_json_path = version_path.join("elm.json");
                                    if !elm_json_path.exists() {
                                        continue;
                                    }

                                    let version_str =
                                        version_entry.file_name().to_string_lossy().to_string();
                                    if let Ok(version) = version_str.parse::<Version>() {
                                        let entry = versions
                                            .entry(pkg_name.clone())
                                            .or_insert_with(Vec::new);
                                        if !entry.contains(&version) {
                                            entry.push(version);
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }

        if !versions.is_empty() {
            info!("Found {} locally installed packages", versions.len());
        }

        Ok(versions)
    }

    fn cache_file() -> Result<File> {
        let mut p_path = Self::packages_path()?;
        p_path.push("elm-json");
        fs::create_dir_all(p_path.clone())?;
        p_path.push("versions.dat");

        OpenOptions::new()
            .write(true)
            .read(true)
            .create(true)
            .open(p_path)
            .map_err(|_| {
                anyhow!("I couldn't open or create the cache file where I cache version info!")
            })
    }

    fn save_cached_versions(
        &self,
        cache_file: &File,
        versions: &HashMap<package::Name, Vec<Version>>,
    ) -> Result<()> {
        let writer = BufWriter::new(cache_file);
        bincode::serialize_into(writer, &versions)?;
        Ok(())
    }

    async fn fetch_remote_versions(
        &self,
        from: usize,
    ) -> Result<HashMap<package::Name, Vec<Version>>> {
        debug!("Fetching versions since {}", from);

        let url = format!("https://package.elm-lang.org/all-packages/since/{}", from);
        let response = reqwest::get(url).await?.text().await?;

        let versions: Vec<String> = serde_json::from_str(&response)?;
        let mut res: HashMap<package::Name, Vec<Version>> = HashMap::new();

        for entry in &versions {
            let parts: Vec<_> = entry.split('@').collect();
            match parts.as_slice() {
                [p, v] => {
                    let name: package::Name = p.parse()?;
                    let version: Version = v.parse()?;
                    let entry = res.entry(name).or_insert_with(Vec::new);
                    entry.push(version)
                }
                _ => bail!("Invalid entry: {}", entry),
            }
        }

        Ok(res)
    }

    pub fn add_preferred_versions<T>(&mut self, versions: T)
    where
        T: IntoIterator<Item = (PackageId, Version)>,
    {
        self.preferred_versions.extend(versions);
    }

    async fn fetch_deps(&mut self, pkg: &Summary) -> Result<Vec<Incompatibility<PackageId>>> {
        debug!("Fetching dependencies for {}@{}", pkg.id, pkg.version);

        if self.offline {
            warn!("Attempting to fetch deps for {:#?}", pkg);
            bail!("I need to fetch dependencies from package.elm-lang.org but I'm working in offline mode!");
        }

        let url = format!(
            "https://package.elm-lang.org/packages/{}/{}/elm.json",
            pkg.id, pkg.version
        );
        let response = reqwest::get(url).await?.text().await?;
        let info: package::Package = serde_json::from_str(&response)?;

        let path = Self::cached_json_path(pkg)?;

        DirBuilder::new()
            .recursive(true)
            .create(path.parent().unwrap())
            .map_err(|_| {
                anyhow!("I tried creating a new folder to cache an elm.json file in but failed!")
            })?;
        let file = OpenOptions::new()
            .write(true)
            .read(true)
            .create(true)
            .open(path.clone())
            .map_err(|_| {
                anyhow!(
                    "I tried an elm.json file here {} but couldn't create or open that location!",
                    path.to_string_lossy()
                )
            })?;
        let mut serializer = serde_json::Serializer::new(file);
        info.serialize(&mut serializer)?;

        Ok(self.deps_from_package(pkg, &info))
    }

    fn read_stored_deps(
        &mut self,
        elm_version: &str,
        extra: &str,
        pkg: &Summary,
    ) -> Result<Vec<Incompatibility<PackageId>>> {
        debug!(
            "Attempting to read stored deps for {}@{}",
            pkg.id, pkg.version
        );

        let mut p_path = Self::packages_path()?;
        p_path.push(format!(
            "{}/package{}/{}/{}/elm.json",
            elm_version, extra, pkg.id, pkg.version
        ));

        let file = File::open(p_path)?;
        let reader = BufReader::new(file);
        let info: package::Package = serde_json::from_reader(reader)?;

        Ok(self.deps_from_package(pkg, &info))
    }

    fn cached_json_path(pkg: &Summary) -> Result<PathBuf> {
        let mut p_path = Self::packages_path()?;
        p_path.push(format!(
            "elm-json/packages/{}/{}/elm.json",
            pkg.id, pkg.version
        ));
        Ok(p_path)
    }

    fn read_cached_deps(&mut self, pkg: &Summary) -> Result<Vec<Incompatibility<PackageId>>> {
        debug!(
            "Attempting to read cached deps for {}@{}",
            pkg.id, pkg.version
        );

        let path = Self::cached_json_path(pkg)?;
        let file = File::open(path)?;
        let reader = BufReader::new(file);
        let info: package::Package = serde_json::from_reader(reader)?;

        Ok(self.deps_from_package(pkg, &info))
    }

    fn deps_from_package(
        &mut self,
        pkg: &Summary,
        info: &package::Package,
    ) -> Vec<Incompatibility<PackageId>> {
        let mut deps: Vec<Incompatibility<_>> = info
            .dependencies
            .iter()
            .map(|(name, range)| {
                let constraint = range.to_constraint().complement();
                Incompatibility::from_dep(pkg.clone(), (name.clone().into(), constraint))
            })
            .collect();

        deps.push(Incompatibility::from_dep(
            pkg.clone(),
            (
                PackageId::Elm,
                info.elm_version().to_constraint().complement(),
            ),
        ));

        debug!("Caching incompatibilities {:#?}", deps);

        self.deps_cache.insert(pkg.clone(), deps.clone());
        deps
    }

    fn packages_path() -> Result<PathBuf> {
        env::var("ELM_HOME")
            .map(PathBuf::from)
            .or_else(|_| {
                if cfg!(windows) {
                    dirs::config_dir()
                        .map(|d| {
                            let mut buf = PathBuf::from(&d);
                            buf.push("elm");
                            buf
                        })
                        .ok_or_else(|| anyhow!("No config directory found?"))
                } else {
                    dirs::home_dir()
                        .map(|h| {
                            let mut buf = PathBuf::from(&h);
                            buf.push(".elm");
                            buf
                        })
                        .ok_or_else(|| anyhow!("No home directory found?"))
                }
            })
            .map_err(|e| anyhow!("{}", e))
    }

    fn root() -> Summary {
        summary::Summary::new(PackageId::Root, Version::new(1, 0, 0))
    }
}

impl retriever::Retriever for Retriever {
    type PackageId = self::PackageId;

    fn root(&self) -> Summary {
        Self::root()
    }

    async fn incompats(&mut self, pkg: &Summary) -> Result<Vec<Incompatibility<Self::PackageId>>> {
        if pkg.id == PackageId::Elm {
            return Ok(Vec::new());
        }
        let from_stored = self
            .deps_cache
            .get(pkg)
            .cloned()
            .ok_or(())
            .or_else(|_| self.read_stored_deps("0.19.0", "", pkg))
            .or_else(|_| self.read_stored_deps("0.19.1", "s", pkg))
            .or_else(|_| self.read_cached_deps(pkg));
        match from_stored {
            Ok(res) => Ok(res),
            Err(_) => self.fetch_deps(pkg).await,
        }
    }

    fn count_versions(&self, pkg: &Self::PackageId) -> usize {
        if let Some(versions) = self.versions.get(pkg) {
            versions.len()
        } else {
            0
        }
    }

    fn best(&mut self, pkg: &Self::PackageId, con: &Constraint) -> Result<Version> {
        debug!(
            "Finding best version for package {} with constraint {}",
            pkg, con
        );
        if let Some(version) = self.preferred_versions.get(pkg) {
            if con.satisfies(version) {
                Ok(*version)
            } else {
                bail!(
                    "I want to use version {} for {} but it's not allowed by constraint {}",
                    version,
                    pkg,
                    con
                )
            }
        } else if let Some(versions) = self.versions.get(pkg) {
            versions
                .iter()
                .filter(|v| con.satisfies(v))
                .max_by(|x, y| match self.mode {
                    Mode::Minimize => y.cmp(x),
                    Mode::Maximize => x.cmp(y),
                })
                .cloned()
                .ok_or_else(|| anyhow!("Failed to find a version for {}", pkg))
        } else {
            bail!("Unknown package {}", pkg)
        }
    }
}
