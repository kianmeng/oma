use std::path::Path;

use rust_apt::{
    cache::{Cache, PackageSort},
    package::Package,
    raw::package::RawPackage,
    records::RecordField,
};

use crate::pkginfo::PkgInfo;

#[derive(Debug, thiserror::Error)]
pub enum OmaDatabaseError {
    #[error(transparent)]
    RustApt(#[from] rust_apt::util::Exception),
    #[error("Invaild pattern: {0}")]
    InvaildPattern(String),
    #[error("Can not find package {0} from database")]
    NoPackage(String),
    #[error("Pkg {0} has no version {1}")]
    NoVersion(String, String),
    #[error("Can not find path for local package {0}")]
    NoPath(String),
}

pub struct OmaDatabase<'a> {
    cache: &'a Cache,
}

pub type OmaDatabaseResult<T> = Result<T, OmaDatabaseError>;

impl<'a> OmaDatabase<'a> {
    pub fn new(cache: &'a Cache) -> OmaDatabaseResult<OmaDatabase<'a>> {
        Ok(Self { cache })
    }

    pub fn query_local_glob(&self, file_glob: &str) -> OmaDatabaseResult<Vec<PkgInfo>> {
        let mut res = vec![];
        let sort = PackageSort::default().only_virtual();

        let glob = self
            .cache
            .packages(&sort)?
            .filter(|x| glob_match::glob_match_with_captures(file_glob, x.name()).is_some())
            .collect::<Vec<_>>();

        for i in glob {
            let real_pkg = real_pkg(&i);
            let pkg = Package::new(self.cache, real_pkg);
            let path = url_no_escape(&format!(
                "file:{}",
                Path::new(i.name())
                    .canonicalize()
                    .map_err(|_| OmaDatabaseError::NoPath(i.name().to_string()))?
                    .to_str()
                    .unwrap_or(pkg.name())
            ))
            .to_string();

            for ver in pkg.versions() {
                let info = PkgInfo::new(self.cache, ver.unique(), &pkg);

                let has = info
                    .apt_sources
                    .iter()
                    .any(|x| url_no_escape(x) == path.as_str());
                if has {
                    res.push(info);
                }
            }
        }

        Ok(res)
    }

    pub fn query_from_glob(
        &self,
        glob: &str,
        filter_candidate: bool,
    ) -> OmaDatabaseResult<Vec<PkgInfo>> {
        let mut res = vec![];
        let sort = PackageSort::default().include_virtual();
        let pkgs = self
            .cache
            .packages(&sort)?
            .filter(|x| glob_match::glob_match_with_captures(glob, x.name()).is_some());

        let pkgs = pkgs
            .map(|x| real_pkg(&x))
            .map(|x| Package::new(&self.cache, x));

        for pkg in pkgs {
            let versions = pkg.versions().collect::<Vec<_>>();
            for ver in versions {
                let pkginfo = PkgInfo::new(&self.cache, ver.unique(), &pkg);
                if filter_candidate && pkginfo.is_candidate {
                    res.push(pkginfo);
                } else if !filter_candidate {
                    res.push(pkginfo);
                }
            }
        }

        Ok(res)
    }

    pub fn query_from_version(&self, pat: &str) -> OmaDatabaseResult<PkgInfo> {
        let (pkgname, version) = pat
            .split_once('=')
            .ok_or_else(|| OmaDatabaseError::InvaildPattern(pat.to_string()))?;

        let pkg = self
            .cache
            .get(pkgname)
            .ok_or_else(|| OmaDatabaseError::NoPackage(pkgname.to_string()))?;

        let version = pkg
            .get_version(version)
            .ok_or_else(|| OmaDatabaseError::NoVersion(pkgname.to_string(), version.to_string()))?
            .unique();

        let info = PkgInfo::new(&self.cache, version, &pkg);

        Ok(info)
    }

    pub fn query_from_branch(
        &self,
        pat: &str,
        filter_candidate: bool,
    ) -> OmaDatabaseResult<Vec<PkgInfo>> {
        let mut res = vec![];
        let (pkgname, branch) = pat
            .split_once('/')
            .ok_or_else(|| OmaDatabaseError::InvaildPattern(pat.to_string()))?;

        let pkg = self
            .cache
            .get(pkgname)
            .ok_or_else(|| OmaDatabaseError::NoPackage(pkgname.to_string()))?;

        let mut sort = vec![];

        for i in pkg.versions() {
            let item = i.get_record(RecordField::Filename);

            if let Some(item) = item {
                if item.split('/').nth(1) == Some(&branch) {
                    sort.push(i)
                }
            }
        }

        sort.sort_by(|x, y| rust_apt::util::cmp_versions(x.version(), y.version()));

        if filter_candidate {
            let version = &sort[sort.len() - 1];
            let oma_pkg = PkgInfo::new(&self.cache, version.unique(), &pkg);
            res.push(oma_pkg);
        } else {
            for i in sort {
                let oma_pkg = PkgInfo::new(&self.cache, i.unique(), &pkg);
                res.push(oma_pkg);
            }
        }

        Ok(res)
    }
}

fn real_pkg(pkg: &Package) -> RawPackage {
    if let Some(provide) = pkg.provides().next() {
        return provide.target_pkg();
    }

    pkg.unique()
}

fn url_no_escape(s: &str) -> String {
    let mut tmp = s.to_string();
    loop {
        let res = url_escape::decode(&tmp);
        let res2 = url_escape::decode(&res);
        if res == res2 {
            return res.to_string();
        } else {
            tmp = res.to_string();
        }
    }
}

#[cfg(test)]
mod test {
    use super::OmaDatabase;
    use rust_apt::new_cache;

    #[test]
    fn test_glob_search() {
        let cache = new_cache!().unwrap();
        let db = OmaDatabase::new(&cache).unwrap();
        let res_filter = db.query_from_glob("apt*", true).unwrap();
        let res = db.query_from_glob("apt*", false).unwrap();

        for i in res_filter {
            println!("{}", i);
        }

        println!("---\n");

        for i in res {
            println!("{}", i);
        }
    }

    #[test]
    fn test_virtual_pkg_search() {
        let cache = new_cache!().unwrap();
        let db = OmaDatabase::new(&cache).unwrap();
        let res_filter = db.query_from_glob("telegram", true).unwrap();

        for i in res_filter {
            println!("{}", i);
        }
    }

    #[test]
    fn test_branch_search() {
        let cache = new_cache!().unwrap();
        let db = OmaDatabase::new(&cache).unwrap();
        let res_filter = db.query_from_branch("apt/stable", true).unwrap();

        for i in res_filter {
            println!("{}", i);
        }
    }
}
