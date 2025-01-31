pub mod transport;

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::{fmt, fs, io};

use git_ref_format::refspec;
use once_cell::sync::Lazy;

use crate::crypto::{Signer, Unverified, Verified};
use crate::git;
use crate::identity;
use crate::identity::project::{Identity, IdentityError};
use crate::identity::{Doc, Id};
use crate::storage::refs;
use crate::storage::refs::{Refs, SignedRefs};
use crate::storage::{
    Error, FetchError, Inventory, ReadRepository, ReadStorage, Remote, Remotes, WriteRepository,
    WriteStorage,
};

pub use crate::git::*;

use super::{RefUpdate, RemoteId};

pub static REMOTES_GLOB: Lazy<refspec::PatternString> =
    Lazy::new(|| refspec::pattern!("refs/remotes/*"));
pub static SIGNATURES_GLOB: Lazy<refspec::PatternString> =
    Lazy::new(|| refspec::pattern!("refs/remotes/*/radicle/signature"));

#[derive(Error, Debug)]
pub enum ProjectError {
    #[error("identity branches diverge from each other")]
    BranchesDiverge,
    #[error("identity branches are in an invalid state")]
    InvalidState,
    #[error("git: {0}")]
    Git(#[from] git2::Error),
    #[error("git: {0}")]
    GitExt(#[from] git::Error),
    #[error("refs: {0}")]
    Refs(#[from] refs::Error),
}

pub struct Storage {
    path: PathBuf,
}

impl fmt::Debug for Storage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Storage(..)")
    }
}

impl ReadStorage for Storage {
    fn path(&self) -> &Path {
        self.path.as_path()
    }

    fn url(&self, proj: &Id) -> Url {
        let path = paths::repository(self, proj);

        Url {
            scheme: git_url::Scheme::File,
            path: path.to_string_lossy().to_string().into(),

            ..git::Url::default()
        }
    }

    fn get(&self, remote: &RemoteId, proj: Id) -> Result<Option<Doc<Verified>>, Error> {
        // TODO: Don't create a repo here if it doesn't exist?
        // Perhaps for checking we could have a `contains` method?
        self.repository(proj)?
            .project_of(remote)
            .map_err(Error::from)
    }

    fn inventory(&self) -> Result<Inventory, Error> {
        self.projects()
    }
}

impl WriteStorage for Storage {
    type Repository = Repository;

    fn repository(&self, proj: Id) -> Result<Self::Repository, Error> {
        Repository::open(paths::repository(self, &proj), proj)
    }

    fn sign_refs<G: Signer>(
        &self,
        repository: &Repository,
        signer: G,
    ) -> Result<SignedRefs<Verified>, Error> {
        repository.sign_refs(signer)
    }

    fn fetch(&self, proj_id: Id, remote: &Url) -> Result<Vec<RefUpdate>, FetchError> {
        let mut repo = self.repository(proj_id).unwrap();
        let mut path = remote.path.clone();

        path.push(b'/');
        path.extend(proj_id.to_string().into_bytes());

        repo.fetch(&Url {
            path,
            ..remote.clone()
        })
    }
}

impl Storage {
    pub fn open<P: AsRef<Path>>(path: P) -> Result<Self, io::Error> {
        let path = path.as_ref().to_path_buf();

        match fs::create_dir_all(&path) {
            Err(err) if err.kind() == io::ErrorKind::AlreadyExists => {}
            Err(err) => return Err(err),
            Ok(()) => {}
        }

        Ok(Self { path })
    }

    pub fn path(&self) -> &Path {
        self.path.as_path()
    }

    pub fn projects(&self) -> Result<Vec<Id>, Error> {
        let mut projects = Vec::new();

        for result in fs::read_dir(&self.path)? {
            let path = result?;
            let id = Id::try_from(path.file_name())?;

            projects.push(id);
        }
        Ok(projects)
    }

    pub fn inspect(&self) -> Result<(), Error> {
        for proj in self.projects()? {
            let repo = self.repository(proj)?;

            for r in repo.raw().references()? {
                let r = r?;
                let name = r.name().ok_or(Error::InvalidRef)?;
                let oid = r.target().ok_or(Error::InvalidRef)?;

                println!("{} {} {}", proj, oid, name);
            }
        }
        Ok(())
    }
}

pub struct Repository {
    pub id: Id,
    pub(crate) backend: git2::Repository,
}

#[derive(Debug, Error)]
pub enum VerifyError {
    #[error("invalid remote `{0}`")]
    InvalidRemote(RemoteId),
    #[error("invalid target `{2}` for reference `{1}` of remote `{0}`")]
    InvalidRefTarget(RemoteId, RefString, git2::Oid),
    #[error("invalid reference")]
    InvalidRef,
    #[error("invalid identity: {0}")]
    InvalidIdentity(#[from] IdentityError),
    #[error("ref error: {0}")]
    Ref(#[from] git::RefError),
    #[error("refs error: {0}")]
    Refs(#[from] refs::Error),
    #[error("unknown reference `{1}` in remote `{0}`")]
    UnknownRef(RemoteId, git::RefString),
    #[error("missing reference `{1}` in remote `{0}`")]
    MissingRef(RemoteId, git::RefString),
    #[error("git: {0}")]
    Git(#[from] git2::Error),
}

impl Repository {
    pub fn open<P: AsRef<Path>>(path: P, id: Id) -> Result<Self, Error> {
        let backend = match git2::Repository::open_bare(path.as_ref()) {
            Err(e) if ext::is_not_found_err(&e) => {
                let backend = git2::Repository::init_opts(
                    path,
                    git2::RepositoryInitOptions::new()
                        .bare(true)
                        .no_reinit(true)
                        .external_template(false),
                )?;
                let mut config = backend.config()?;

                // TODO: Get ahold of user name and/or key.
                config.set_str("user.name", "radicle")?;
                config.set_str("user.email", "radicle@localhost")?;

                Ok(backend)
            }
            Ok(repo) => Ok(repo),
            Err(e) => Err(e),
        }?;

        Ok(Self { id, backend })
    }

    pub fn head(&self) -> Result<git2::Commit, git2::Error> {
        // TODO: Find longest history, get document and get head.
        // Perhaps we should even set a local `HEAD` or at least `refs/heads/master`
        todo!();
    }

    pub fn verify(&self) -> Result<(), VerifyError> {
        let mut remotes: HashMap<RemoteId, Refs> = self
            .remotes()?
            .map(|remote| {
                let (id, remote) = remote?;
                Ok((id, remote.refs.into()))
            })
            .collect::<Result<_, VerifyError>>()?;

        for r in self.backend.references()? {
            let r = r?;
            let name = r.name().ok_or(VerifyError::InvalidRef)?;
            let oid = r.target().ok_or(VerifyError::InvalidRef)?;
            let (remote_id, refname) = git::parse_ref::<RemoteId>(name)?;

            if refname == *refs::SIGNATURE_REF {
                continue;
            }
            let remote = remotes
                .get_mut(&remote_id)
                .ok_or(VerifyError::InvalidRemote(remote_id))?;
            let signed_oid = remote
                .remove(&refname)
                .ok_or_else(|| VerifyError::UnknownRef(remote_id, refname.clone()))?;

            if Oid::from(oid) != signed_oid {
                return Err(VerifyError::InvalidRefTarget(remote_id, refname, oid));
            }
        }

        for (remote, refs) in remotes.into_iter() {
            // The refs that are left in the map, are ones that were signed, but are not
            // in the repository.
            if let Some((name, _)) = refs.into_iter().next() {
                return Err(VerifyError::MissingRef(remote, name));
            }
            // Verify identity history of remote.
            self.identity(&remote)?.verified(self.id)?;
        }

        Ok(())
    }

    pub fn inspect(&self) -> Result<(), Error> {
        for r in self.backend.references()? {
            let r = r?;
            let name = r.name().ok_or(Error::InvalidRef)?;
            let oid = r.target().ok_or(Error::InvalidRef)?;

            println!("{} {}", oid, name);
        }
        Ok(())
    }

    pub fn identity(&self, remote: &RemoteId) -> Result<Identity<Oid>, IdentityError> {
        Identity::load(remote, self)
    }

    pub fn project_of(
        &self,
        remote: &RemoteId,
    ) -> Result<Option<identity::Doc<Verified>>, refs::Error> {
        if let Some((doc, _)) = identity::Doc::load(remote, self)? {
            Ok(Some(doc.verified().unwrap()))
        } else {
            Ok(None)
        }
    }

    /// Return the canonical identity [`git::Oid`] and document.
    pub fn project(&self) -> Result<(Oid, identity::Doc<Unverified>), ProjectError> {
        let mut heads = Vec::new();
        for remote in self.remote_ids()? {
            let remote = remote?;
            let oid = Doc::<Unverified>::head(&remote, self)?.unwrap();

            heads.push(oid.into());
        }
        // Keep track of the longest identity branch.
        let mut longest = heads.pop().ok_or(ProjectError::InvalidState)?;

        for head in &heads {
            let base = self.raw().merge_base(*head, longest)?;

            if base == longest {
                // `head` is a successor of `longest`. Update `longest`.
                //
                //   o head
                //   |
                //   o longest (base)
                //   |
                //
                longest = *head;
            } else if base == *head || *head == longest {
                // `head` is an ancestor of `longest`, or equal to it. Do nothing.
                //
                //   o longest             o longest, head (base)
                //   |                     |
                //   o head (base)   OR    o
                //   |                     |
                //
            } else {
                // The merge base between `head` and `longest` (`base`)
                // is neither `head` nor `longest`. Therefore, the branches have
                // diverged.
                //
                //    longest   head
                //           \ /
                //            o (base)
                //            |
                //
                return Err(ProjectError::BranchesDiverge);
            }
        }

        Doc::load_at(longest.into(), self)?
            .ok_or(refs::Error::NotFound)
            .map(|(doc, _)| (longest.into(), doc))
            .map_err(ProjectError::from)
    }

    pub fn remote_ids(
        &self,
    ) -> Result<impl Iterator<Item = Result<RemoteId, refs::Error>> + '_, git2::Error> {
        let iter = self.backend.references_glob(SIGNATURES_GLOB.as_str())?.map(
            |reference| -> Result<RemoteId, refs::Error> {
                let r = reference?;
                let name = r.name().ok_or(refs::Error::InvalidRef)?;
                let (id, _) = git::parse_ref::<RemoteId>(name)?;

                Ok(id)
            },
        );
        Ok(iter)
    }

    pub fn remotes(
        &self,
    ) -> Result<
        impl Iterator<Item = Result<(RemoteId, Remote<Verified>), refs::Error>> + '_,
        git2::Error,
    > {
        let remotes = self.backend.references_glob(SIGNATURES_GLOB.as_str())?.map(
            |reference| -> Result<_, _> {
                let r = reference?;
                let name = r.name().ok_or(refs::Error::InvalidRef)?;
                let (id, _) = git::parse_ref::<RemoteId>(name)?;
                let remote = self.remote(&id)?;

                Ok((id, remote))
            },
        );
        Ok(remotes)
    }

    pub fn sign_refs<G: Signer>(&self, signer: G) -> Result<SignedRefs<Verified>, Error> {
        let remote = signer.public_key();
        let refs = self.references(remote)?;
        let signed = refs.signed(&signer)?;

        signed.save(remote, self)?;

        Ok(signed)
    }
}

impl ReadRepository for Repository {
    fn is_empty(&self) -> Result<bool, git2::Error> {
        let some = self.remotes()?.next().is_some();
        Ok(!some)
    }

    fn path(&self) -> &Path {
        self.backend.path()
    }

    fn blob_at<'a>(&'a self, oid: Oid, path: &'a Path) -> Result<git2::Blob<'a>, git::Error> {
        git::ext::Blob::At {
            object: oid.into(),
            path,
        }
        .get(&self.backend)
    }

    fn reference(
        &self,
        remote: &RemoteId,
        name: &git::RefStr,
    ) -> Result<Option<git2::Reference>, git2::Error> {
        let name = name.strip_prefix(git::refname!("refs")).unwrap_or(name);
        let name = format!("refs/remotes/{remote}/{name}");
        self.backend.find_reference(&name).map(Some).or_else(|e| {
            if git::ext::is_not_found_err(&e) {
                Ok(None)
            } else {
                Err(e)
            }
        })
    }

    fn commit(&self, oid: Oid) -> Result<Option<git2::Commit>, git2::Error> {
        self.backend.find_commit(oid.into()).map(Some).or_else(|e| {
            if git::ext::is_not_found_err(&e) {
                Ok(None)
            } else {
                Err(e)
            }
        })
    }

    fn revwalk(&self, head: Oid) -> Result<git2::Revwalk, git2::Error> {
        let mut revwalk = self.backend.revwalk()?;
        revwalk.push(head.into())?;

        Ok(revwalk)
    }

    fn reference_oid(
        &self,
        remote: &RemoteId,
        reference: &git::RefStr,
    ) -> Result<Option<Oid>, git2::Error> {
        let reference = self.reference(remote, reference)?;
        Ok(reference.and_then(|r| r.target().map(|o| o.into())))
    }

    fn remote(&self, remote: &RemoteId) -> Result<Remote<Verified>, refs::Error> {
        let refs = SignedRefs::load(remote, self)?;
        Ok(Remote::new(*remote, refs))
    }

    fn references(&self, remote: &RemoteId) -> Result<Refs, Error> {
        // TODO: Only return known refs, eg. heads/ rad/ tags/ etc..
        let entries = self
            .backend
            .references_glob(format!("refs/remotes/{remote}/*").as_str())?;
        let mut refs = BTreeMap::new();

        for e in entries {
            let e = e?;
            let name = e.name().ok_or(Error::InvalidRef)?;
            let (_, refname) = git::parse_ref::<RemoteId>(name)?;
            let oid = e.target().ok_or(Error::InvalidRef)?;

            refs.insert(refname, oid.into());
        }
        Ok(refs.into())
    }

    fn remotes(&self) -> Result<Remotes<Verified>, refs::Error> {
        let mut remotes = Vec::new();
        for remote in Repository::remotes(self)? {
            remotes.push(remote?);
        }
        Ok(Remotes::from_iter(remotes))
    }

    fn project(&self) -> Result<Doc<Verified>, Error> {
        todo!()
    }

    fn project_identity(&self) -> Result<(Oid, identity::Doc<Unverified>), ProjectError> {
        Repository::project(self)
    }
}

impl WriteRepository for Repository {
    /// Fetch all remotes of a project from the given URL.
    /// This is the primary way in which projects are updated on the network.
    ///
    /// Since we're operating in an untrusted network, we have to be take some precautions
    /// when fetching from a remote. We don't want to fetch straight into a public facing
    /// repository because if the updates were to be invalid, we'd be allowing others to
    /// read this invalid state. We also don't want to lock our repositories during the fetch
    /// or verification, as this will make the repositories unavailable. Therefore, we choose
    /// to perform the fetch into a "staging" copy of the given repository we're fetching, and
    /// then transfer the changes to the canonical, public copy of the repository.
    ///
    /// To do this, we first create a temporary directory, and clone the canonical repo into it.
    /// This local clone takes advantage of the fact that both repositories live on the same
    /// host (or even file-system). We now have a "staging" copy and the canonical copy.
    ///
    /// We then fetch the *remote* repo into the *staging* copy. We turn off pruning because we
    /// don't want to accidentally delete any objects before verification is complete.
    ///
    /// We proceed to verify the staging copy through the usual verification process.
    ///
    /// If verification succeeds, we fetch from the staging copy into the canonical repo,
    /// with pruning *on*, and discard the staging copy. If it fails, we just discard the
    /// staging copy.
    ///
    fn fetch(&mut self, url: &git::Url) -> Result<Vec<RefUpdate>, FetchError> {
        // TODO: Have function to fetch specific remotes.
        //
        // The steps are summarized in the following diagram:
        //
        //     staging <- git-clone -- local (canonical) # create staging copy
        //     staging <- git-fetch -- remote            # fetch from remote
        //
        //     ... verify ...
        //
        //     local <- git-fetch -- staging             # fetch from staging copy
        //
        let url = url.to_string();
        let refs: &[&str] = &["refs/remotes/*:refs/remotes/*"];
        let mut updates = Vec::new();
        let mut callbacks = git2::RemoteCallbacks::new();
        let tempdir = tempfile::tempdir()?;

        // Create staging copy.
        let staging = {
            let mut builder = git2::build::RepoBuilder::new();
            let path = tempdir.path().join("git");
            let staging_repo = builder
                .bare(true)
                // Using `clone_local` will try to hard-link the ODBs for better performance.
                // TODO: Due to this, I think we'll have to run GC when there is a failure.
                .clone_local(git2::build::CloneLocal::Local)
                .clone(
                    &git::Url {
                        scheme: git::url::Scheme::File,
                        path: self.backend.path().to_string_lossy().to_string().into(),
                        ..git::Url::default()
                    }
                    .to_string(),
                    &path,
                )?;

            // In case we fetch an invalid update, we want to make sure nothing is deleted.
            let mut opts = git2::FetchOptions::default();
            opts.prune(git2::FetchPrune::Off);

            // Fetch from the remote into the staging copy.
            staging_repo
                .remote_anonymous(&url)?
                .fetch(refs, Some(&mut opts), None)?;

            // Verify the staging copy as if it was the canonical copy.
            Repository {
                id: self.id,
                backend: staging_repo,
            }
            .verify()?;

            path
        };

        callbacks.update_tips(|name, old, new| {
            if let Ok(name) = git::RefString::try_from(name) {
                updates.push(RefUpdate::from(name, old, new));
            } else {
                log::warn!("Invalid ref `{}` detected; aborting fetch", name);
                return false;
            }
            // Returning `true` ensures the process is not aborted.
            true
        });

        {
            let mut remote = self.backend.remote_anonymous(
                &git::Url {
                    scheme: git::url::Scheme::File,
                    path: staging.to_string_lossy().to_string().into(),
                    ..git::Url::default()
                }
                .to_string(),
            )?;
            let mut opts = git2::FetchOptions::default();
            opts.remote_callbacks(callbacks);

            // TODO: Make sure we verify before pruning, as pruning may get us into
            // a state we can't roll back.
            opts.prune(git2::FetchPrune::On);
            // Fetch from the staging copy into the canonical repo.
            remote.fetch(refs, Some(&mut opts), None)?;
        }

        Ok(updates)
    }

    fn raw(&self) -> &git2::Repository {
        &self.backend
    }
}

pub mod trailers {
    use std::str::FromStr;

    use super::*;
    use crate::crypto::{PublicKey, PublicKeyError};
    use crate::crypto::{Signature, SignatureError};

    pub const SIGNATURE_TRAILER: &str = "Rad-Signature";

    #[derive(Error, Debug)]
    pub enum Error {
        #[error("invalid format for signature trailer")]
        SignatureTrailerFormat,
        #[error("invalid public key in signature trailer")]
        PublicKey(#[from] PublicKeyError),
        #[error("invalid signature in trailer")]
        Signature(#[from] SignatureError),
    }

    pub fn parse_signatures(msg: &str) -> Result<Vec<(PublicKey, Signature)>, Error> {
        let trailers =
            git2::message_trailers_strs(msg).map_err(|_| Error::SignatureTrailerFormat)?;
        let mut signatures = Vec::with_capacity(trailers.len());

        for (key, val) in trailers.iter() {
            if key == SIGNATURE_TRAILER {
                if let Some((pk, sig)) = val.split_once(' ') {
                    let pk = PublicKey::from_str(pk)?;
                    let sig = Signature::from_str(sig)?;

                    signatures.push((pk, sig));
                } else {
                    return Err(Error::SignatureTrailerFormat);
                }
            }
        }
        Ok(signatures)
    }
}

pub mod paths {
    use std::path::PathBuf;

    use super::Id;
    use super::ReadStorage;

    pub fn repository<S: ReadStorage>(storage: &S, proj: &Id) -> PathBuf {
        storage.path().join(proj.to_string())
    }
}

#[cfg(test)]
mod tests {
    use std::io::{Read, Write};
    use std::{io, net, process, thread};

    use super::*;
    use crate::assert_matches;
    use crate::git;
    use crate::rad;
    use crate::storage::refs::SIGNATURE_REF;
    use crate::storage::{ReadRepository, ReadStorage, RefUpdate, WriteRepository};
    use crate::test::arbitrary;
    use crate::test::fixtures;
    use crate::test::signer::MockSigner;

    #[test]
    fn test_remote_refs() {
        let dir = tempfile::tempdir().unwrap();
        let signer = MockSigner::default();
        let storage = fixtures::storage(dir.path(), &signer).unwrap();
        let inv = storage.inventory().unwrap();
        let proj = inv.first().unwrap();
        let mut refs = git::remote_refs(&git::Url {
            scheme: git_url::Scheme::File,
            path: paths::repository(&storage, proj)
                .to_string_lossy()
                .into_owned()
                .into(),
            ..git::Url::default()
        })
        .unwrap();

        let project = storage.repository(*proj).unwrap();
        let remotes = project.remotes().unwrap();

        // Strip the remote refs of sigrefs so we can compare them.
        for remote in refs.values_mut() {
            remote.remove(&*SIGNATURE_REF).unwrap();
        }

        let remotes = remotes
            .map(|remote| remote.map(|(id, r): (RemoteId, Remote<Verified>)| (id, r.refs.into())))
            .collect::<Result<_, _>>()
            .unwrap();

        assert_eq!(refs, remotes);
    }

    #[test]
    fn test_fetch() {
        let tmp = tempfile::tempdir().unwrap();
        let alice_signer = MockSigner::default();
        let alice = fixtures::storage(tmp.path().join("alice"), alice_signer).unwrap();
        let bob = Storage::open(tmp.path().join("bob")).unwrap();
        let inventory = alice.inventory().unwrap();
        let proj = *inventory.first().unwrap();
        let repo = alice.repository(proj).unwrap();
        let remotes = repo.remotes().unwrap().collect::<Vec<_>>();
        let refname = git::refname!("heads/master");

        // Have Bob fetch Alice's refs.
        let updates = bob
            .repository(proj)
            .unwrap()
            .fetch(&git::Url {
                scheme: git_url::Scheme::File,
                path: paths::repository(&alice, &proj)
                    .to_string_lossy()
                    .into_owned()
                    .into(),
                ..git::Url::default()
            })
            .unwrap();

        // Four refs are created for each remote.
        assert_eq!(updates.len(), remotes.len() * 3);

        for update in updates {
            assert_matches!(
                update,
                RefUpdate::Created { name, .. } if name.starts_with("refs/remotes")
            );
        }

        for remote in remotes {
            let (id, _) = remote.unwrap();
            let alice_repo = alice.repository(proj).unwrap();
            let alice_oid = alice_repo.reference(&id, &refname).unwrap().unwrap();

            let bob_repo = bob.repository(proj).unwrap();
            let bob_oid = bob_repo.reference(&id, &refname).unwrap().unwrap();

            assert_eq!(alice_oid.target(), bob_oid.target());
        }
    }

    #[test]
    fn test_fetch_update() {
        let tmp = tempfile::tempdir().unwrap();
        let alice = Storage::open(tmp.path().join("alice/storage")).unwrap();
        let bob = Storage::open(tmp.path().join("bob/storage")).unwrap();

        let alice_signer = MockSigner::new(&mut fastrand::Rng::new());
        let alice_id = alice_signer.public_key();
        let (proj_id, _, proj_repo, alice_head) =
            fixtures::project(tmp.path().join("alice/project"), &alice, &alice_signer).unwrap();

        let refname = git::refname!("refs/heads/master");
        let alice_url = git::Url {
            scheme: git_url::Scheme::File,
            path: paths::repository(&alice, &proj_id)
                .to_string_lossy()
                .into_owned()
                .into(),
            ..git::Url::default()
        };

        // Have Bob fetch Alice's refs.
        let updates = bob.repository(proj_id).unwrap().fetch(&alice_url).unwrap();
        // Three refs are created: the branch, the signature and the id.
        assert_eq!(updates.len(), 3);

        let alice_proj_storage = alice.repository(proj_id).unwrap();
        let alice_head = proj_repo.find_commit(alice_head).unwrap();
        let alice_head = git::commit(&proj_repo, &alice_head, &refname, "Making changes", "Alice")
            .unwrap()
            .id();
        git::push(&proj_repo).unwrap();
        alice.sign_refs(&alice_proj_storage, &alice_signer).unwrap();

        // Have Bob fetch Alice's new commit.
        let updates = bob.repository(proj_id).unwrap().fetch(&alice_url).unwrap();
        // The branch and signature refs are updated.
        assert_matches!(
            updates.as_slice(),
            &[RefUpdate::Updated { .. }, RefUpdate::Updated { .. }]
        );

        // Bob's storage is updated.
        let bob_repo = bob.repository(proj_id).unwrap();
        let bob_master = bob_repo.reference(alice_id, &refname).unwrap().unwrap();

        assert_eq!(bob_master.target().unwrap(), alice_head);
    }

    #[test]
    fn test_upload_pack() {
        let tmp = tempfile::tempdir().unwrap();
        let signer = MockSigner::default();
        let remote = *signer.public_key();
        let storage = Storage::open(tmp.path().join("storage")).unwrap();
        let socket = net::TcpListener::bind(net::SocketAddr::from(([0, 0, 0, 0], 0))).unwrap();
        let addr = socket.local_addr().unwrap();
        let source_path = tmp.path().join("source");
        let target_path = tmp.path().join("target");
        let (source, _) = fixtures::repository(&source_path);
        let (proj, _) = rad::init(
            &source,
            "radicle",
            "radicle",
            git::refname!("master"),
            signer,
            &storage,
        )
        .unwrap();

        let t = thread::spawn(move || {
            let (stream, _) = socket.accept().unwrap();
            let repo = storage.repository(proj).unwrap();
            // NOTE: `GIT_PROTOCOL=version=2` doesn't work.
            let mut child = process::Command::new("git")
                .current_dir(repo.path())
                .arg("upload-pack")
                .arg("--strict") // The path to the git repo must be exact.
                .arg(".")
                .stdout(process::Stdio::piped())
                .stdin(process::Stdio::piped())
                .spawn()
                .unwrap();

            let mut stdin = child.stdin.take().unwrap();
            let mut stdout = child.stdout.take().unwrap();

            let mut stream_r = stream.try_clone().unwrap();
            let mut stream_w = stream;

            let t = thread::spawn(move || {
                let mut buf = [0u8; 1024];

                while let Ok(n) = stream_r.read(&mut buf) {
                    if n == 0 {
                        break;
                    }
                    if stdin.write_all(&buf[..n]).is_err() {
                        break;
                    }
                }
            });
            io::copy(&mut stdout, &mut stream_w).unwrap();

            t.join().unwrap();
            child.wait().unwrap();
        });

        let mut updates = Vec::new();
        {
            let mut callbacks = git2::RemoteCallbacks::new();
            let mut opts = git2::FetchOptions::default();

            callbacks.update_tips(|name, _, _| {
                updates.push(name.to_owned());
                true
            });
            opts.remote_callbacks(callbacks);

            // Register the `rad://` transport.
            transport::register().unwrap();

            let target = git2::Repository::init_bare(target_path).unwrap();
            let stream = net::TcpStream::connect(addr).unwrap();
            let smart = transport::Smart::singleton();

            smart.insert(proj, Box::new(stream.try_clone().unwrap()));

            // Fetch with the `rad://` transport.
            target
                .remote_anonymous(&format!("rad://{}", proj))
                .unwrap()
                .fetch(&["refs/*:refs/*"], Some(&mut opts), None)
                .unwrap();

            stream.shutdown(net::Shutdown::Both).unwrap();

            t.join().unwrap();
        }

        assert_eq!(
            updates,
            vec![
                format!("refs/remotes/{remote}/heads/master"),
                format!("refs/remotes/{remote}/heads/radicle/id"),
                format!("refs/remotes/{remote}/radicle/signature")
            ]
        );
    }

    #[test]
    fn test_sign_refs() {
        let tmp = tempfile::tempdir().unwrap();
        let mut rng = fastrand::Rng::new();
        let signer = MockSigner::new(&mut rng);
        let storage = Storage::open(tmp.path()).unwrap();
        let proj_id = arbitrary::gen::<Id>(1);
        let alice = *signer.public_key();
        let project = storage.repository(proj_id).unwrap();
        let backend = &project.backend;
        let sig = git2::Signature::now(&alice.to_string(), "anonymous@radicle.xyz").unwrap();
        let head = git::initial_commit(backend, &sig).unwrap();

        git::commit(
            backend,
            &head,
            &git::RefString::try_from(format!("refs/remotes/{alice}/heads/master")).unwrap(),
            "Second commit",
            &alice.to_string(),
        )
        .unwrap();

        let signed = storage.sign_refs(&project, &signer).unwrap();
        let remote = project.remote(&alice).unwrap();
        let mut unsigned = project.references(&alice).unwrap();

        // The signed refs doesn't contain the signature ref itself.
        unsigned.remove(&*SIGNATURE_REF).unwrap();

        assert_eq!(remote.refs, signed);
        assert_eq!(*remote.refs, unsigned);
    }
}
