//! Processus : lancement par le gestionnaire de processus (init), qui
//! trouve le programme dans l'image de demarrage ou sur la carte SD
//! (`/bin/NOM`, ou un chemin), puis attente et arret par le handle rendu.
//!
//! Entree et sorties : la console (heritee), rien (`null`), ou des tubes
//! (`piped`, `io::pipe`, sortie d'un autre enfant) transmis au gestionnaire
//! avec la requete. Un parent lui-meme redirige transmet ses propres tubes.
//! Seule la redirection vers un fichier (`Stdio::from(File)`) manque.

use super::env::{CommandEnv, CommandEnvs, CommandResolvedEnvs};
pub use crate::ffi::OsString as EnvKey;
use crate::ffi::{OsStr, OsString};
use crate::num::NonZero;
use crate::path::Path;
use crate::process::StdioPipes;
use crate::sys::fs::File;
use crate::sys::pal::abi::err::{E2BIG, EBADF, ENOSYS, ETIMEDOUT};
use crate::sys::pal::abi::proto::pm;
use crate::sys::pal::abi::{spawn, startup, timeout};
use crate::sys::pal::ipc;
use crate::sys::pal::{cvt, startup_flags, svc, syscall, unsupported};
use crate::{fmt, io};

////////////////////////////////////////////////////////////////////////////////
// Command
////////////////////////////////////////////////////////////////////////////////

pub struct Command {
    program: OsString,
    args: Vec<OsString>,
    env: CommandEnv,

    cwd: Option<OsString>,
    stdin: Option<Stdio>,
    stdout: Option<Stdio>,
    stderr: Option<Stdio>,
}

#[derive(Debug)]
pub enum Stdio {
    Inherit,
    Null,
    MakePipe,
    ParentStdout,
    ParentStderr,
    #[allow(dead_code)] // This variant exists only for the Debug impl
    InheritFile(File),
    /// Tube existant : `io::pipe`, ou cote d'un tube d'un autre enfant.
    Pipe(ChildPipe),
}

/// Flux standard du point de vue de l'enfant : 0 entree, 1 sortie, 2 erreurs.
const PIPE_BITS: [u64; 3] = [spawn::STDIN_PIPE, spawn::STDOUT_PIPE, spawn::STDERR_PIPE];
const NULL_BITS: [u64; 3] = [spawn::STDIN_NULL, spawn::STDOUT_NULL, spawn::STDERR_NULL];

/// Ce que le processus courant a lui-meme recu pour le flux `k` : un tube
/// (a transmettre tel quel), rien, ou la console (aucun drapeau).
fn own_stream(k: usize) -> (u64, Option<u32>) {
    let f = startup_flags();
    let (pipe, null, h) = match k {
        0 => (startup::STDIN_PIPE, startup::STDIN_NULL, svc::stdin()),
        1 => (startup::STDOUT_PIPE, startup::STDOUT_NULL, svc::console()),
        _ => (startup::STDERR_PIPE, startup::STDERR_NULL, svc::stderr()),
    };
    if f & null != 0 {
        (NULL_BITS[k], None)
    } else if f & pipe != 0 {
        (PIPE_BITS[k], Some(h))
    } else {
        (0, None)
    }
}

impl Command {
    pub fn new(program: &OsStr) -> Command {
        Command {
            program: program.to_owned(),
            args: vec![program.to_owned()],
            env: Default::default(),
            cwd: None,
            stdin: None,
            stdout: None,
            stderr: None,
        }
    }

    pub fn arg(&mut self, arg: &OsStr) {
        self.args.push(arg.to_owned());
    }

    pub fn env_mut(&mut self) -> &mut CommandEnv {
        &mut self.env
    }

    pub fn cwd(&mut self, dir: &OsStr) {
        self.cwd = Some(dir.to_owned());
    }

    pub fn stdin(&mut self, stdin: Stdio) {
        self.stdin = Some(stdin);
    }

    pub fn stdout(&mut self, stdout: Stdio) {
        self.stdout = Some(stdout);
    }

    pub fn stderr(&mut self, stderr: Stdio) {
        self.stderr = Some(stderr);
    }

    pub fn get_program(&self) -> &OsStr {
        &self.program
    }

    pub fn get_args(&self) -> CommandArgs<'_> {
        let mut iter = self.args.iter();
        iter.next();
        CommandArgs { iter }
    }

    pub fn get_envs(&self) -> CommandEnvs<'_> {
        self.env.iter()
    }

    pub fn get_env_clear(&self) -> bool {
        self.env.does_clear()
    }

    pub fn get_resolved_envs(&self) -> CommandResolvedEnvs {
        CommandResolvedEnvs::new(self.env.capture())
    }

    pub fn get_current_dir(&self) -> Option<&Path> {
        self.cwd.as_ref().map(|cs| Path::new(cs))
    }

    /// Chaines de lancement : arguments (le premier est le programme), puis
    /// environnement « NOM=valeur » (dont `PWD`, le dossier courant).
    fn strings(&self) -> io::Result<(Vec<u8>, u32, u32)> {
        let mut data = Vec::new();
        // Chemin relatif (« ./outil », « bin/x ») : absolu depuis le dossier
        // courant, comme le fait le shell ; un nom seul est cherche par init.
        let prog = self.program.as_encoded_bytes();
        if prog.contains(&b'/') && !prog.starts_with(b"/") {
            data.extend_from_slice(crate::sys::fs::nxk_absolute(Path::new(&self.program))?.as_bytes());
        } else {
            data.extend_from_slice(prog);
        }
        data.push(0);
        for a in &self.args[1..] {
            data.extend_from_slice(a.as_encoded_bytes());
            data.push(0);
        }
        let mut vars: Vec<Vec<u8>> = match self.env.capture_if_changed() {
            None => crate::sys::env::pairs(),
            Some(map) => map
                .into_iter()
                .map(|(k, v)| {
                    let mut s = k.into_encoded_bytes();
                    s.push(b'=');
                    s.extend_from_slice(v.as_encoded_bytes());
                    s
                })
                .collect(),
        };
        if let Some(dir) = &self.cwd {
            let abs = crate::sys::fs::nxk_absolute(Path::new(dir))?;
            vars.retain(|v| !v.starts_with(b"PWD="));
            vars.push(format!("PWD={abs}").into_bytes());
        }
        let envc = vars.len() as u32;
        for v in vars {
            if v.contains(&0) {
                return Err(io::const_error!(io::ErrorKind::InvalidInput, "octet nul dans l'environnement"));
            }
            data.extend_from_slice(&v);
            data.push(0);
        }
        if self.args.iter().any(|a| a.as_encoded_bytes().contains(&0)) {
            return Err(io::const_error!(io::ErrorKind::InvalidInput, "octet nul dans un argument"));
        }
        Ok((data, self.args.len() as u32, envc))
    }

    pub fn spawn(
        &mut self,
        default: Stdio,
        needs_stdin: bool,
    ) -> io::Result<(Process, StdioPipes)> {
        let h = svc::pm();
        if h == 0 {
            return Err(io::Error::from_raw_os_error(ENOSYS as i32));
        }
        let (strings, argc, envc) = self.strings()?;
        // Sans entree demandee (`output`), l'enfant lit un flux vide.
        let stdin_default = if needs_stdin { &default } else { &Stdio::Null };
        let cfgs = [
            self.stdin.as_ref().unwrap_or(stdin_default),
            self.stdout.as_ref().unwrap_or(&default),
            self.stderr.as_ref().unwrap_or(&default),
        ];
        let mut flags = 0u64;
        let mut grants: Vec<u32> = Vec::new();
        // Cotes gardes par le parent, et cotes de l'enfant crees ici (fermes
        // une fois transmis).
        let mut ours: [Option<ChildPipe>; 3] = [None, None, None];
        let mut theirs: Vec<ChildPipe> = Vec::new();
        for (k, cfg) in cfgs.into_iter().enumerate() {
            let (bits, handle) = match cfg {
                Stdio::Inherit => own_stream(k),
                // Sortie du parent : seulement pour les sorties de l'enfant.
                Stdio::ParentStdout if k > 0 => own_stream(1),
                Stdio::ParentStderr if k > 0 => own_stream(2),
                Stdio::ParentStdout | Stdio::ParentStderr => (0, None),
                Stdio::Null => (NULL_BITS[k], None),
                Stdio::MakePipe => {
                    let (r, w) = crate::sys::pipe::pipe()?;
                    // L'enfant lit son entree et ecrit ses sorties.
                    let (child, parent) = if k == 0 { (r, w) } else { (w, r) };
                    let h = child.handle();
                    theirs.push(child);
                    ours[k] = Some(parent);
                    (PIPE_BITS[k], Some(h))
                }
                Stdio::Pipe(p) => {
                    if p.is_reader() != (k == 0) {
                        return Err(io::const_error!(io::ErrorKind::InvalidInput, "tube dans le mauvais sens"));
                    }
                    (PIPE_BITS[k], Some(p.handle()))
                }
                Stdio::InheritFile(_) => return unsupported(),
            };
            // Flux du parent d'une autre nature (sa sortie pour une entree) :
            // on l'exprime avec les drapeaux du flux `k`.
            let bits = match bits {
                0 => 0,
                b if PIPE_BITS.contains(&b) => PIPE_BITS[k],
                _ => NULL_BITS[k],
            };
            flags |= bits;
            if let Some(h) = handle {
                grants.push(h);
            }
        }
        let mut payload = Vec::with_capacity(16 + strings.len());
        for w in [argc, envc, flags as u32, 0] {
            payload.extend_from_slice(&w.to_le_bytes());
        }
        payload.extend_from_slice(&strings);
        let (m, n) = ipc::pack(pm::SPAWN, &payload);
        if n < payload.len() {
            return Err(io::Error::from_raw_os_error(E2BIG as i32));
        }
        for (i, g) in grants.iter().enumerate() {
            ipc::set_send_handle(i, *g);
        }
        svc::call(h, &m.with_handles(grants.len()))?;
        let handle = ipc::received_handle(0);
        drop(theirs);
        let [stdin, stdout, stderr] = ours;
        Ok((Process { handle }, StdioPipes { stdin, stdout, stderr }))
    }
}

impl From<ChildPipe> for Stdio {
    fn from(pipe: ChildPipe) -> Stdio {
        Stdio::Pipe(pipe)
    }
}

impl From<io::Stdout> for Stdio {
    fn from(_: io::Stdout) -> Stdio {
        Stdio::ParentStdout
    }
}

impl From<io::Stderr> for Stdio {
    fn from(_: io::Stderr) -> Stdio {
        Stdio::ParentStderr
    }
}

impl From<File> for Stdio {
    fn from(file: File) -> Stdio {
        Stdio::InheritFile(file)
    }
}

impl fmt::Debug for Command {
    // show all attributes
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if f.alternate() {
            let mut debug_command = f.debug_struct("Command");
            debug_command.field("program", &self.program).field("args", &self.args);
            if !self.env.is_unchanged() {
                debug_command.field("env", &self.env);
            }

            if self.cwd.is_some() {
                debug_command.field("cwd", &self.cwd);
            }

            if self.stdin.is_some() {
                debug_command.field("stdin", &self.stdin);
            }
            if self.stdout.is_some() {
                debug_command.field("stdout", &self.stdout);
            }
            if self.stderr.is_some() {
                debug_command.field("stderr", &self.stderr);
            }

            debug_command.finish()
        } else {
            if let Some(ref cwd) = self.cwd {
                write!(f, "cd {cwd:?} && ")?;
            }
            if self.env.does_clear() {
                write!(f, "env -i ")?;
                // Altered env vars will be printed next, that should exactly work as expected.
            } else {
                // Removed env vars need the command to be wrapped in `env`.
                let mut any_removed = false;
                for (key, value_opt) in self.get_envs() {
                    if value_opt.is_none() {
                        if !any_removed {
                            write!(f, "env ")?;
                            any_removed = true;
                        }
                        write!(f, "-u {} ", key.to_string_lossy())?;
                    }
                }
            }
            // Altered env vars can just be added in front of the program.
            for (key, value_opt) in self.get_envs() {
                if let Some(value) = value_opt {
                    write!(f, "{}={value:?} ", key.to_string_lossy())?;
                }
            }
            if self.program != self.args[0] {
                write!(f, "[{:?}] ", self.program)?;
            }
            write!(f, "{:?}", self.args[0])?;

            for arg in &self.args[1..] {
                write!(f, " {:?}", arg)?;
            }
            Ok(())
        }
    }
}

/// Code de sortie rendu par le noyau (`process_exit`, ou 137 pour un
/// processus tue, 134 pour un abandon).
#[derive(PartialEq, Eq, Clone, Copy, Debug, Default)]
pub struct ExitStatus(i64);

impl ExitStatus {
    pub fn exit_ok(&self) -> Result<(), ExitStatusError> {
        match NonZero::new(self.0) {
            None => Ok(()),
            Some(c) => Err(ExitStatusError(c)),
        }
    }

    pub fn code(&self) -> Option<i32> {
        Some(self.0 as i32)
    }
}

impl fmt::Display for ExitStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "exit status: {}", self.0)
    }
}

#[derive(PartialEq, Eq, Clone, Copy, Debug)]
pub struct ExitStatusError(NonZero<i64>);

impl Into<ExitStatus> for ExitStatusError {
    fn into(self) -> ExitStatus {
        ExitStatus(self.0.get())
    }
}

impl ExitStatusError {
    pub fn code(self) -> Option<NonZero<i32>> {
        NonZero::new(self.0.get() as i32)
    }
}

#[derive(PartialEq, Eq, Clone, Copy, Debug)]
pub struct ExitCode(u8);

impl ExitCode {
    pub const SUCCESS: ExitCode = ExitCode(0);
    pub const FAILURE: ExitCode = ExitCode(1);

    pub fn as_i32(&self) -> i32 {
        self.0 as i32
    }
}

impl From<u8> for ExitCode {
    fn from(code: u8) -> Self {
        Self(code)
    }
}

pub struct Process {
    handle: u32,
}

impl Process {
    pub fn id(&self) -> u32 {
        syscall::getpid(self.handle).max(0) as u32
    }

    pub fn kill(&mut self) -> io::Result<()> {
        cvt(syscall::process_kill(self.handle)).map(|_| ())
    }

    fn wait_for(&mut self, t: u64) -> io::Result<Option<ExitStatus>> {
        let r = syscall::process_wait(self.handle, t);
        if r == -ETIMEDOUT {
            Ok(None)
        } else if r == -EBADF {
            Err(io::Error::from_raw_os_error(EBADF as i32))
        } else {
            Ok(Some(ExitStatus(r)))
        }
    }

    pub fn wait(&mut self) -> io::Result<ExitStatus> {
        Ok(self.wait_for(timeout::INFINITE)?.unwrap_or_default())
    }

    pub fn try_wait(&mut self) -> io::Result<Option<ExitStatus>> {
        self.wait_for(timeout::POLL)
    }
}

impl Drop for Process {
    fn drop(&mut self) {
        syscall::handle_close(self.handle);
    }
}

pub struct CommandArgs<'a> {
    iter: crate::slice::Iter<'a, OsString>,
}

impl<'a> Iterator for CommandArgs<'a> {
    type Item = &'a OsStr;
    fn next(&mut self) -> Option<&'a OsStr> {
        self.iter.next().map(|os| &**os)
    }
    fn size_hint(&self) -> (usize, Option<usize>) {
        self.iter.size_hint()
    }
}

impl<'a> ExactSizeIterator for CommandArgs<'a> {
    fn len(&self) -> usize {
        self.iter.len()
    }
    fn is_empty(&self) -> bool {
        self.iter.is_empty()
    }
}

impl<'a> fmt::Debug for CommandArgs<'a> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_list().entries(self.iter.clone()).finish()
    }
}

pub type ChildPipe = crate::sys::pipe::Pipe;

/// Lit les deux sorties d'un enfant en meme temps : un tube sans tampon
/// bloquerait l'enfant sur l'une pendant qu'on attendrait l'autre.
pub fn read_output(
    out: ChildPipe,
    stdout: &mut Vec<u8>,
    err: ChildPipe,
    stderr: &mut Vec<u8>,
) -> io::Result<()> {
    let h = crate::thread::Builder::new().spawn(move || {
        let mut v = Vec::new();
        err.read_to_end(&mut v).map(|_| v)
    })?;
    let r = out.read_to_end(stdout);
    let e = h.join().map_err(|_| io::const_error!(io::ErrorKind::Other, "lecture des erreurs"))?;
    stderr.extend_from_slice(&e?);
    r.map(|_| ())
}

pub fn getpid() -> u32 {
    syscall::getpid(0).max(0) as u32
}
