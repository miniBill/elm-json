use super::{incompat::Incompatibility, summary};
use crate::semver::{Constraint, Version};
use anyhow::Result;

pub trait Retriever {
    type PackageId: summary::PackageId;

    fn root(&self) -> summary::Summary<Self::PackageId>;
    fn incompats(
        &mut self,
        pkg: &summary::Summary<Self::PackageId>,
    ) -> impl std::future::Future<Output = Result<Vec<Incompatibility<Self::PackageId>>>> + Send;
    fn count_versions(&self, pkg: &Self::PackageId) -> usize;
    fn best(&mut self, pkg: &Self::PackageId, con: &Constraint) -> Result<Version>;
}
