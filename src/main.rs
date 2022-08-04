use fuser::{
    FileAttr, FileType, Filesystem, ReplyAttr, ReplyData, ReplyDirectory, ReplyEmpty, ReplyEntry,
    ReplyOpen, Request,
};
use libc::{
    c_int, exit, fork, setsid, EINVAL, EIO, ENOENT, EPERM, O_ACCMODE, O_APPEND, O_CREAT, O_EXCL,
    O_RDONLY, O_RDWR, O_TRUNC, O_WRONLY,
};
use log::{debug, error, warn};
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    env,
    ffi::{OsStr, OsString},
    fmt::{Debug, Formatter},
    fs::File,
    io::{BufReader, BufWriter, Error, ErrorKind},
    ops::Deref,
    os::unix::{
        ffi::OsStrExt,
        fs::{MetadataExt, PermissionsExt},
    },
    path::{Path, PathBuf},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

type Result<T> = std::result::Result<T, Error>;
type SerdeResult<T> = std::result::Result<T, Box<dyn std::error::Error>>;

const TTL: Duration = Duration::from_secs(120);

#[derive(Serialize, Deserialize)]
#[serde(remote = "FileType")]
enum FileTypeDef {
    NamedPipe,
    CharDevice,
    BlockDevice,
    Directory,
    RegularFile,
    Symlink,
    Socket,
}

#[derive(Serialize, Deserialize)]
#[serde(remote = "FileAttr")]
struct FileAttrDef {
    pub ino: u64,
    pub size: u64,
    pub blocks: u64,
    pub atime: SystemTime,
    pub mtime: SystemTime,
    pub ctime: SystemTime,
    pub crtime: SystemTime,
    #[serde(with = "FileTypeDef")]
    pub kind: FileType,
    pub perm: u16,
    pub nlink: u32,
    pub uid: u32,
    pub gid: u32,
    pub rdev: u32,
    pub flags: u32,
    pub blksize: u32,
}

#[derive(Serialize, Deserialize)]
enum TypeExtra {
    RegularFile,
    Symlink(OsString),
    Directory(HashMap<OsString, u64>),
}

#[derive(Serialize, Deserialize)]
struct FileInfo {
    parent: u64,
    path: PathBuf,
    #[serde(with = "FileAttrDef")]
    attr: FileAttr,
    type_extra: TypeExtra,
}

#[derive(Default, Serialize, Deserialize)]
struct FileTree {
    inode_to_path: HashMap<u64, FileInfo>,
}

impl Debug for FileTree {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        writeln!(f, "FileTree")?;
        // dumb way to always print this in inode order
        let mut inode_to_path = std::collections::BTreeMap::default();
        inode_to_path.extend(self.inode_to_path.iter());
        for (key, val) in inode_to_path.iter() {
            writeln!(
                f,
                "-- {key}: [parent: {}, {:?}, {:?}]",
                val.parent, val.attr.kind, val.path
            )?;
            match &val.type_extra {
                TypeExtra::Directory(children) => writeln!(f, "---- children: {:?}", children)?,
                TypeExtra::Symlink(link) => writeln!(f, "---- link to: {:?}", link)?,
                TypeExtra::RegularFile => (),
            }
        }
        Ok(())
    }
}

impl FileTree {
    fn load_or_build(root_path: &Path, cache_path: &Path) -> SerdeResult<Self> {
        let path = cache_path.join("cache-fs.tree.zst");
        match FileTree::load(&path) {
            Ok(tree) => return Ok(tree),
            Err(e) => warn!("error loading {:?}: {:?}", path, e),
        }
        let tree = FileTree::build(root_path);
        tree.save(&path)?;
        Ok(tree)
    }

    fn load(path: &Path) -> SerdeResult<Self> {
        let file = File::open(path)?;
        let file = BufReader::new(file);
        let file = zstd::stream::Decoder::new(file)?;

        Ok(bincode::deserialize_from(file)?)
    }

    fn save(&self, path: &Path) -> SerdeResult<()> {
        let file = File::create(path)?;
        let file = BufWriter::new(file);
        let file = zstd::stream::Encoder::new(file, 9)?.auto_finish();

        Ok(bincode::serialize_into(file, self)?)
    }

    fn build(root_path: &Path) -> Self {
        let mut tree = FileTree::default();

        let mut ino = 1;
        let root = FileInfo {
            parent: 0, // probably should be None but this is the only file without a parent
            path: PathBuf::new(),
            attr: std::fs::symlink_metadata(root_path)
                .and_then(|m| meta2attr(&m, ino))
                .expect("cannot read root dir"),
            type_extra: TypeExtra::Directory(Default::default()),
        };
        tree.inode_to_path.insert(1, root);
        ino += 1;

        let mut dirs = vec![1];
        while !dirs.is_empty() {
            let mut all_dirs = Vec::new();
            for dir in dirs {
                tree.process_dir(root_path, &mut ino, &mut all_dirs, dir);
            }
            dirs = all_dirs;
        }

        debug!("build tree: {:?}", tree);
        tree
    }

    fn process_dir(
        &mut self,
        root_path: &Path,
        ino_counter: &mut u64,
        dirs: &mut Vec<u64>,
        ino: u64,
    ) {
        let dir = self
            .inode_to_path
            .get_mut(&ino)
            .expect("missing dir ino, programming error");
        if let Ok(x) = std::fs::read_dir(root_path.join(&dir.path)) {
            if let TypeExtra::Directory(children) = &mut dir.type_extra {
                children.reserve(x.size_hint().0);
            } else {
                panic!("impossible")
            };
            let dir_path = dir.path.clone();
            for de in x.flatten() {
                if let Ok(attr) = de.metadata().and_then(|m| meta2attr(&m, *ino_counter)) {
                    let path = dir_path.join(de.file_name());
                    let type_extra = match attr.kind {
                        FileType::RegularFile => TypeExtra::RegularFile,
                        FileType::Directory => {
                            dirs.push(attr.ino);
                            TypeExtra::Directory(Default::default())
                        }
                        FileType::Symlink => {
                            let entry_path = root_path.join(&path);
                            match std::fs::read_link(entry_path) {
                                Err(e) => {
                                    // I guess on error we just ignore this symlink like it doesn't exist
                                    error!("bad symlink? {:?}", e);
                                    continue;
                                }
                                Ok(x) => TypeExtra::Symlink(x.into_os_string()),
                            }
                        }
                        _ => panic!("impossible to happen, we filter other types out"),
                    };
                    let child = FileInfo {
                        parent: ino,
                        path,
                        attr,
                        type_extra,
                    };
                    // avoid this lookup each time with something better?
                    if let Some(TypeExtra::Directory(children)) =
                        &mut self.inode_to_path.get_mut(&ino).map(|f| &mut f.type_extra)
                    {
                        children.insert(de.file_name(), child.attr.ino);
                    } else {
                        unreachable!("this should be impossible");
                    }
                    self.inode_to_path.insert(child.attr.ino, child);
                    *ino_counter += 1;
                }
            }
        }
    }

    pub fn lookup(&self, parent: u64, child: &OsStr) -> Option<&FileAttr> {
        let (_, children) = self.folder(parent)?;
        let child = children.get(child)?;
        let child = self.inode_to_path.get(child)?;
        Some(&child.attr)
    }

    pub fn getattr(&self, ino: u64) -> Option<&FileAttr> {
        Some(&self.inode_to_path.get(&ino)?.attr)
    }

    pub fn folder(&self, ino: u64) -> Option<(&FileInfo, &HashMap<OsString, u64>)> {
        self.inode_to_path.get(&ino).and_then(|f| {
            if let TypeExtra::Directory(children) = &f.type_extra {
                Some((f, children))
            } else {
                None
            }
        })
    }

    pub fn symlink(&self, ino: u64) -> Option<(&FileInfo, &OsString)> {
        self.inode_to_path.get(&ino).and_then(|f| {
            if let TypeExtra::Symlink(link) = &f.type_extra {
                Some((f, link))
            } else {
                None
            }
        })
    }

    pub fn file(&self, ino: u64) -> Option<&FileInfo> {
        self.inode_to_path.get(&ino)
    }
}

#[derive(Debug)]
struct FileHandle {
    file: File,
    count: usize,
}

impl FileHandle {
    fn new(file: File) -> Self {
        FileHandle { file, count: 1 }
    }

    fn open(&mut self) {
        self.count += 1;
    }

    fn close(&mut self) -> bool {
        self.count -= 1;
        self.count == 0
    }
}

impl Deref for FileHandle {
    type Target = File;

    fn deref(&self) -> &Self::Target {
        &self.file
    }
}

struct CacheFs {
    remote_dir: PathBuf,
    cache_dir: PathBuf,
    cache_tmp_file: PathBuf,
    tree: FileTree,
    opened_files: HashMap<u64, FileHandle>,
    read_buffer: Vec<u8>,
}

impl CacheFs {
    pub fn new(remote_dir: PathBuf, cache_dir: PathBuf, tree: FileTree) -> CacheFs {
        CacheFs {
            remote_dir,
            cache_dir: cache_dir.join("root"),
            cache_tmp_file: cache_dir.join("tmp.file"),
            tree,
            opened_files: HashMap::with_capacity(2),
            read_buffer: Vec::with_capacity(4096),
        }
    }
}

fn ft2ft(t: std::fs::FileType) -> Result<FileType> {
    match t {
        x if x.is_symlink() => Ok(FileType::Symlink),
        x if x.is_dir() => Ok(FileType::Directory),
        x if x.is_file() => Ok(FileType::RegularFile),
        _ => Err(Error::from(ErrorKind::NotFound)),
    }
}

fn meta2attr(m: &std::fs::Metadata, ino: u64) -> Result<FileAttr> {
    Ok(FileAttr {
        kind: ft2ft(m.file_type())?,
        ino,
        size: m.size(),
        blocks: m.blocks(),
        atime: m.accessed().unwrap_or(UNIX_EPOCH),
        mtime: m.modified().unwrap_or(UNIX_EPOCH),
        ctime: UNIX_EPOCH + Duration::from_secs(m.ctime().try_into().unwrap_or(0)),
        crtime: m.created().unwrap_or(UNIX_EPOCH),
        perm: m.permissions().mode() as u16,
        nlink: m.nlink() as u32,
        uid: m.uid(),
        gid: m.gid(),
        rdev: m.rdev() as u32,
        flags: 0,
        blksize: m.blksize() as u32,
    })
}

fn errhandle(e: Error) -> libc::c_int {
    match e.kind() {
        ErrorKind::PermissionDenied => EPERM,
        ErrorKind::NotFound => ENOENT,
        e => {
            error!("{:?}", e);
            EIO
        }
    }
}

impl Filesystem for CacheFs {
    fn lookup(&mut self, _req: &Request, parent: u64, name: &OsStr, reply: ReplyEntry) {
        debug!("lookup: parent: {parent}, name: {:?}", name);
        match self.tree.lookup(parent, name) {
            None => reply.error(ENOENT),
            Some(attr) => reply.entry(&TTL, attr, 1),
        }
    }

    fn getattr(&mut self, _req: &Request, ino: u64, reply: ReplyAttr) {
        debug!("getattr: ino: {ino}");
        match self.tree.getattr(ino) {
            None => reply.error(ENOENT),
            Some(attr) => reply.attr(&TTL, attr),
        }
    }

    fn open(&mut self, _req: &Request, ino: u64, flags: i32, reply: ReplyOpen) {
        debug!("open: ino: {ino}, flags: {flags}");

        if let Some(file_handle) = self.opened_files.get_mut(&ino) {
            file_handle.open();
            return reply.opened(ino, 0);
        }

        let entry_path = match self.tree.file(ino) {
            None => return reply.error(ENOENT),
            Some(file) => &file.path,
        };

        debug!("open: entry_path: {:?}", entry_path);

        let fl = flags as c_int;

        if !matches!(fl & O_ACCMODE, O_RDONLY | O_WRONLY | O_RDWR) {
            return reply.error(EINVAL);
        }

        if (fl & (O_EXCL | O_CREAT) != 0) || fl & O_APPEND == O_APPEND || fl & O_TRUNC == O_TRUNC {
            error!("Wrong flags on open");
            return reply.error(EIO);
        }

        let mut oo = std::fs::OpenOptions::new();
        oo.read(true);
        oo.write(false);
        oo.create(false);
        oo.append(false);
        oo.truncate(false);

        let cache_path = self.cache_dir.join(&entry_path);
        if !&cache_path.exists() {
            // copy the file into place
            // todo: handle these errors
            if let Some(parent) = cache_path.parent() {
                if let Err(e) = std::fs::create_dir_all(parent) {
                    error!("cannot create cache dir {:?} to copy into: {:?}", parent, e);
                    return reply.error(EIO);
                }
            }
            let remote_path = self.remote_dir.join(entry_path);
            debug!(
                "copying from {:?} to {:?}",
                remote_path, self.cache_tmp_file
            );
            if let Err(e) = std::fs::copy(&remote_path, &self.cache_tmp_file) {
                error!(
                    "failed to copy from {:?} to {:?}: {:?}",
                    &remote_path, self.cache_tmp_file, e
                );
                return reply.error(EIO);
            }
            debug!("moving from {:?} to {:?}", self.cache_tmp_file, cache_path);
            if let Err(e) = std::fs::rename(&self.cache_tmp_file, &cache_path) {
                error!(
                    "failed to move from {:?} to {:?}: {:?}",
                    self.cache_tmp_file, cache_path, e
                );
                // try to delete it in case it partially moved or something (shouldn't happen, should always be atomic)
                // but ignore any error deleting it because what could we do anyway?
                std::fs::remove_file(cache_path).ok();
                return reply.error(EIO);
            }
        }

        match oo.open(cache_path) {
            Err(e) => reply.error(errhandle(e)),
            Ok(f) => {
                self.opened_files.insert(ino, FileHandle::new(f));
                reply.opened(ino, 0);
            }
        }
    }

    fn read(
        &mut self,
        _req: &Request<'_>,
        ino: u64,
        fh: u64,
        offset: i64,
        size: u32,
        _flags: i32,
        _lock_owner: Option<u64>,
        reply: ReplyData,
    ) {
        debug!("read: ino: {ino}, fh: {fh}, offset: {offset}, size: {size}");
        let f = match self.opened_files.get(&fh) {
            None => return reply.error(EIO),
            Some(x) => x,
        };

        let size = size as usize;

        let b = &mut self.read_buffer;
        if b.len() != size {
            b.resize(size, 0);
        }

        use std::os::unix::fs::FileExt;

        let mut bo = 0;
        while bo < size {
            match f.read_at(&mut b[bo..], offset as u64) {
                Err(e) => return reply.error(errhandle(e)),
                Ok(0) => {
                    b.resize(bo, 0);
                    break;
                }
                Ok(ret) => {
                    bo += ret;
                }
            };
        }

        reply.data(&b[..]);
    }

    fn release(
        &mut self,
        _req: &Request<'_>,
        ino: u64,
        fh: u64,
        _flags: i32,
        _lock_owner: Option<u64>,
        _flush: bool,
        reply: ReplyEmpty,
    ) {
        debug!("release: ino: {ino}, fh: {fh}");
        // we have 2 choices here:
        // 1. optimize for many simultaneously opened files in which case we'd get_mut, and then remove if required
        // 2. optimize for normally only 1 simultaneously opened file, so removing and then only adding back if keeping is best
        // we pick #2
        let mut file_handle = match self.opened_files.remove(&fh) {
            None => return reply.error(EIO),
            Some(x) => x,
        };

        if !file_handle.close() {
            self.opened_files.insert(fh, file_handle);
        }

        reply.ok();
    }

    fn opendir(&mut self, _req: &Request, ino: u64, flags: i32, reply: ReplyOpen) {
        debug!("opendir: ino: {ino}, flags: {flags}");
        match self.tree.getattr(ino) {
            None => reply.error(ENOENT),
            Some(attr) => reply.opened(attr.ino, 0),
        }
    }

    fn readdir(
        &mut self,
        _req: &Request,
        ino: u64,
        fh: u64,
        offset: i64,
        mut reply: ReplyDirectory,
    ) {
        debug!("readdir: ino: {ino}, fh: {fh}, offset: {offset}");

        let (dir, children) = match self.tree.folder(ino) {
            None => return reply.error(EIO),
            Some(x) => x,
        };

        if offset == 0
            && reply.add(
                dir.attr.ino,
                1,
                FileType::Directory,
                OsStr::from_bytes(b"."),
            )
        {
            return reply.ok();
        }

        if offset <= 1 && reply.add(dir.parent, 2, FileType::Directory, OsStr::from_bytes(b"..")) {
            return reply.ok();
        }

        let offset = if offset <= 1 { 0 } else { offset as usize - 2 };

        for (i, (name, ino)) in children.iter().enumerate().skip(offset) {
            let file = match self.tree.file(*ino) {
                Some(file) => file,
                None => {
                    error!("should be impossible to not be able to find a child");
                    return reply.error(EIO);
                }
            };
            // i + 3 means the index of the next entry
            let offset = (i + 3) as i64;
            debug!(
                "sending ino: {}, offset: {}, kind: {:?}, name: {:?}",
                *ino, offset, file.attr.kind, name
            );
            if reply.add(*ino, offset, file.attr.kind, name) {
                break;
            }
        }
        reply.ok();
    }

    fn releasedir(&mut self, _req: &Request, ino: u64, fh: u64, flags: i32, reply: ReplyEmpty) {
        debug!("releasedir: ino: {ino}, fh: {fh}, flags: {flags}");
        // or could just always return ok() ?
        match self.tree.file(ino) {
            None => reply.error(EIO),
            Some(_) => reply.ok(),
        };
    }

    fn readlink(&mut self, _req: &Request, ino: u64, reply: ReplyData) {
        debug!("readlink: ino: {ino}");
        let (_, link) = match self.tree.symlink(ino) {
            None => return reply.error(ENOENT),
            Some(x) => x,
        };
        reply.data(link.as_bytes());
    }
}

pub fn daemon() {
    unsafe {
        match fork() {
            // child
            0 => {
                if setsid() == -1 {
                    error!("error executing setsid()");
                    exit(1);
                }
            }
            -1 => {
                error!("error executing fork()");
                exit(1);
            }
            // parent
            _ => exit(0),
        }
    }
}

fn main() {
    env_logger::init();
    let mut args = env::args_os().skip(1);
    let mut cmd_opts = "ro".to_string();
    let mut cache_dir = "".to_string();
    let mut default_permissions = true;
    let mut fork_daemon = true;

    let mut count = 0;
    let mut pos_args = [None, None];

    while let Some(arg) = args.next() {
        if arg == "-o" {
            let opts = args.next().expect("found -o but missing opts");
            let opts = opts.to_str().expect("non-utf8 opts").split(',');
            for opt in opts {
                if opt.starts_with("cache_dir=") {
                    let mut split_opt = opt.splitn(2, '=');
                    if let Some(dir) = split_opt.nth(1) {
                        cache_dir.clear();
                        cache_dir.push_str(dir);
                    }
                    continue;
                }
                match opt {
                    "ro" => (),
                    "no_default_permissions" => default_permissions = false,
                    "no_daemon" | "no_fork" | "nodaemon" | "nofork" => fork_daemon = false,
                    "rw" => panic!("rw is not supported"),
                    opt => {
                        cmd_opts.push(',');
                        cmd_opts.push_str(opt);
                    }
                }
            }
            if cache_dir.is_empty() {
                panic!("must supply cache_dir=/path/to/cache to -o")
            }
            if !cmd_opts.contains(",fsname=") {
                cmd_opts.push_str(",fsname=cachefs");
            }
            if default_permissions && !cmd_opts.contains(",default_permissions,") {
                cmd_opts.push_str(",default_permissions");
            }
        } else if count < pos_args.len() {
            pos_args[count] = Some(arg);
            count += 1
        } else {
            panic!("too many arguments");
        }
    }

    let remote_dir = PathBuf::from(pos_args[0].as_ref().expect("missing dir"));
    let mountpoint = pos_args[1].as_ref().expect("missing mountpoint");
    let cache_dir = PathBuf::from(cache_dir);

    debug!(
        "mounting {:?} on {:?} with cache_dir: {:?}, opts: {cmd_opts}",
        remote_dir, mountpoint, cache_dir
    );

    std::fs::create_dir_all(&cache_dir).expect("could not create cache_dir");
    let tree = FileTree::load_or_build(remote_dir.deref(), cache_dir.deref())
        .expect("could not build file tree");

    let cache = CacheFs::new(remote_dir, cache_dir, tree);

    let cmd_opts = OsString::from(cmd_opts);
    let options = [OsStr::new("-o"), cmd_opts.as_os_str()];

    if fork_daemon {
        daemon();
    }
    #[allow(deprecated)]
    fuser::mount(cache, mountpoint, &options).expect("mount failed");
}
