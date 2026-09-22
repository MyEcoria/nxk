//! Interface binaire du micro-noyau nxk.
//!
//! Partage par le noyau et par les programmes utilisateur : numeros d'appels
//! systeme, codes d'erreur, droits, disposition memoire et formats d'echange.
//! Une seule source de verite, pour que les deux cotes ne puissent pas diverger.
//!
//! Conventions d'appel, identiques a Linux arm64 : `svc #0`, numero dans `x8`,
//! arguments dans `x0`-`x5`, resultat dans `x0`, erreur sous forme de `-errno`.
//! Les appels IPC utilisent en plus `x1`-`x7` pour le message, et rendent
//! l'insigne de l'emetteur dans `x8`, un jeton de reponse dans `x9` et le
//! pid de l'emetteur dans `x10` (0 pour une notification).

#![no_std]

/// Numeros d'appels systeme.
///
/// Attentes bornees : les appels bloquants prennent un delai relatif en
/// nanosecondes (`timeout::INFINITE` = 0 : sans limite). Un delai echu avant
/// meme de bloquer fait echouer l'appel avec `-ETIMEDOUT` (`timeout::POLL`).
pub mod sys {
    pub const NOP: u64 = 0;
    pub const DEBUG_WRITE: u64 = 1;
    pub const YIELD: u64 = 2;
    pub const THREAD_EXIT: u64 = 3;
    pub const PROCESS_EXIT: u64 = 4;
    pub const SLEEP: u64 = 5;
    /// clock(id) : `clock::MONOTONIC` ou `clock::REALTIME`, en ns.
    pub const CLOCK: u64 = 6;
    pub const GETCPU: u64 = 7;
    /// getrandom(buf, len) : octets aleatoires (ChaCha20 du noyau) ; rend le
    /// nombre d'octets ecrits (au plus 64 KiB par appel).
    pub const GETRANDOM: u64 = 8;
    /// clock_set(sysctl, id, ns) : regle l'heure murale (capacite SysControl).
    pub const CLOCK_SET: u64 = 9;

    pub const HANDLE_CLOSE: u64 = 10;
    pub const HANDLE_DUP: u64 = 11;
    /// handle_revoke(h) : invalide toutes les autres capacites designant le
    /// meme objet (endpoint, notification) ; rend un handle neuf. Droit CONTROL.
    pub const HANDLE_REVOKE: u64 = 12;
    /// getpid(process) : identifiant du processus designe par un handle, ou
    /// du processus appelant (0).
    pub const GETPID: u64 = 13;
    /// gettid() : identifiant du thread appelant (celui du debogage).
    pub const GETTID: u64 = 14;

    pub const MEM_MAP: u64 = 20;
    pub const MEM_UNMAP: u64 = 21;
    pub const MMIO_MAP: u64 = 22;
    pub const MMIO_DERIVE: u64 = 23;
    /// mem_protect(addr, len, prot) : change les droits de pages projetees.
    pub const MEM_PROTECT: u64 = 24;
    /// shm_create(len, flags) : objet de memoire partagee (`shm::*`).
    pub const SHM_CREATE: u64 = 25;
    /// shm_map(h, addr, prot) : projette tout l'objet (addr 0 = choix du noyau).
    pub const SHM_MAP: u64 = 26;
    /// shm_paddr(h, offset) : adresse physique d'une page (droit DMA).
    pub const SHM_PADDR: u64 = 27;

    pub const THREAD_CREATE: u64 = 30;
    /// thread_set(h, what, value) : `thread::SET_*` ; h = 0 : thread courant.
    pub const THREAD_SET: u64 = 31;

    /// futex_wait(addr, val, timeout) : bloque si `*addr == val` (32 bits).
    /// Cle = adresse physique : fonctionne aussi en memoire partagee.
    pub const FUTEX_WAIT: u64 = 35;
    /// futex_wake(addr, n) : reveille jusqu'a n attentes ; rend leur nombre.
    pub const FUTEX_WAKE: u64 = 36;

    pub const PROCESS_SPAWN: u64 = 40;
    /// process_wait(h, timeout) : code de sortie.
    pub const PROCESS_WAIT: u64 = 41;
    pub const PROCESS_KILL: u64 = 42;

    /// endpoint_create(flags) : `endpoint::PIPE` pour un tube.
    pub const ENDPOINT_CREATE: u64 = 50;
    /// Les appels IPC bloquants lisent leur delai dans x9.
    pub const IPC_CALL: u64 = 51;
    pub const IPC_SEND: u64 = 52;
    pub const IPC_RECV: u64 = 53;
    pub const IPC_REPLY: u64 = 54;
    pub const IPC_REPLY_RECV: u64 = 55;
    pub const IPC_REPLY_TO: u64 = 56;

    pub const NOTIFY_CREATE: u64 = 60;
    pub const NOTIFY_SIGNAL: u64 = 61;
    /// notify_wait(h, timeout).
    pub const NOTIFY_WAIT: u64 = 62;
    pub const NOTIFY_BIND: u64 = 63;

    pub const IRQ_GET: u64 = 70;
    pub const IRQ_BIND: u64 = 71;
    pub const IRQ_ACK: u64 = 72;

    pub const SYSINFO: u64 = 80;
    /// system_power(sysctl, cmd) : `power::REBOOT` ou `power::OFF`. Sans
    /// moniteur (PSCI), l'extinction passe par le PMIC : service utilisateur.
    pub const SYSTEM_POWER: u64 = 81;

    /// debug_attach(process, endpoint) : les fautes et points d'arret du
    /// processus deviennent des messages sur l'endpoint (droit CONTROL).
    pub const DEBUG_ATTACH: u64 = 90;
    /// debug_regs(process, tid, buf, write) : lit/ecrit `debug::Regs`.
    pub const DEBUG_REGS: u64 = 91;
    /// debug_mem(process, addr, buf, len | write << 63).
    pub const DEBUG_MEM: u64 = 92;
    /// debug_threads(process, buf, max) : identifiants des threads.
    pub const DEBUG_THREADS: u64 = 93;
    /// debug_stop(process, tid) : arrete un thread (0 : tous) ; chacun produit
    /// un evenement STOP. Sans debogueur, les threads restent arretes jusqu'a
    /// l'attachement.
    pub const DEBUG_STOP: u64 = 94;
}

/// Drapeaux de `endpoint_create`.
pub mod endpoint {
    /// Tube : un ecrivain (handle SEND sans RECV) envoie ses donnees par
    /// `ipc_call` (etiquette `proto::console::WRITE`), un lecteur (handle
    /// RECV) les recoit. Quand il ne reste plus aucun ecrivain, la reception
    /// rend -EPIPE : fin du flux. Sans lecteur, l'envoi rend -EPIPE.
    pub const PIPE: u64 = 1 << 0;
}

/// Delais des appels bloquants (nanosecondes relatives).
pub mod timeout {
    pub const INFINITE: u64 = 0;
    /// Ne pas attendre : echec immediat (-ETIMEDOUT) si l'appel devait bloquer.
    pub const POLL: u64 = 1;
}

/// Commandes de `system_power`.
pub mod power {
    pub const OFF: u64 = 0;
    pub const REBOOT: u64 = 1;
}

/// Horloges de `clock`.
pub mod clock {
    /// Temps depuis le demarrage, jamais ajuste.
    pub const MONOTONIC: u64 = 0;
    /// Heure murale (ns depuis 1970), reglee par le service d'horloge.
    pub const REALTIME: u64 = 1;
}

/// Drapeaux de `shm_create`.
pub mod shm {
    /// Pages physiquement contigues (tampons DMA sans IOMMU), 4 MiB au plus.
    pub const CONTIGUOUS: u64 = 1 << 0;
    /// Projection non cachee (Normal-NC) : coherente avec les peripheriques
    /// qui ne sondent pas les caches du processeur.
    pub const UNCACHED: u64 = 1 << 1;
    /// Pages sous 4 GiB (peripheriques a adressage DMA sur 32 bits).
    pub const DMA32: u64 = 1 << 2;
}

/// Codes d'erreur : valeurs errno de Linux, renvoyees en negatif. Les
/// services (fichiers, reseau) utilisent les memes, si bien que la
/// bibliotheque standard retrouve ses `ErrorKind` habituels.
pub mod err {
    pub const EPERM: i64 = 1;
    pub const ENOENT: i64 = 2;
    pub const ESRCH: i64 = 3;
    pub const EINTR: i64 = 4;
    pub const EIO: i64 = 5;
    pub const ENXIO: i64 = 6;
    pub const E2BIG: i64 = 7;
    pub const ENOEXEC: i64 = 8;
    pub const EBADF: i64 = 9;
    pub const ECHILD: i64 = 10;
    pub const EAGAIN: i64 = 11;
    pub const ENOMEM: i64 = 12;
    pub const EACCES: i64 = 13;
    pub const EFAULT: i64 = 14;
    pub const EBUSY: i64 = 16;
    pub const EEXIST: i64 = 17;
    pub const EXDEV: i64 = 18;
    pub const ENODEV: i64 = 19;
    pub const ENOTDIR: i64 = 20;
    pub const EISDIR: i64 = 21;
    pub const EINVAL: i64 = 22;
    pub const ENFILE: i64 = 23;
    pub const EMFILE: i64 = 24;
    pub const ENOTTY: i64 = 25;
    pub const EFBIG: i64 = 27;
    pub const ENOSPC: i64 = 28;
    pub const ESPIPE: i64 = 29;
    pub const EROFS: i64 = 30;
    pub const EMLINK: i64 = 31;
    pub const EPIPE: i64 = 32;
    pub const ERANGE: i64 = 34;
    pub const EDEADLK: i64 = 35;
    pub const ENAMETOOLONG: i64 = 36;
    pub const ENOSYS: i64 = 38;
    pub const ENOTEMPTY: i64 = 39;
    pub const ELOOP: i64 = 40;
    pub const ENODATA: i64 = 61;
    pub const EOVERFLOW: i64 = 75;
    pub const EILSEQ: i64 = 84;
    pub const ENOTSOCK: i64 = 88;
    pub const EDESTADDRREQ: i64 = 89;
    pub const EMSGSIZE: i64 = 90;
    pub const EPROTOTYPE: i64 = 91;
    pub const ENOPROTOOPT: i64 = 92;
    pub const EPROTONOSUPPORT: i64 = 93;
    pub const EOPNOTSUPP: i64 = 95;
    pub const EAFNOSUPPORT: i64 = 97;
    pub const EADDRINUSE: i64 = 98;
    pub const EADDRNOTAVAIL: i64 = 99;
    pub const ENETDOWN: i64 = 100;
    pub const ENETUNREACH: i64 = 101;
    pub const ECONNABORTED: i64 = 103;
    pub const ECONNRESET: i64 = 104;
    pub const ENOBUFS: i64 = 105;
    pub const EISCONN: i64 = 106;
    pub const ENOTCONN: i64 = 107;
    pub const ESHUTDOWN: i64 = 108;
    pub const ETIMEDOUT: i64 = 110;
    pub const ECONNREFUSED: i64 = 111;
    pub const EHOSTUNREACH: i64 = 113;
    pub const EALREADY: i64 = 114;
    pub const EINPROGRESS: i64 = 115;
    /// Synonyme de EAGAIN, comme sous Linux.
    pub const EWOULDBLOCK: i64 = EAGAIN;

    pub fn name(e: i64) -> &'static str {
        match e.unsigned_abs() as i64 {
            EPERM => "EPERM",
            ENOENT => "ENOENT",
            ESRCH => "ESRCH",
            EINTR => "EINTR",
            EIO => "EIO",
            ENXIO => "ENXIO",
            E2BIG => "E2BIG",
            ENOEXEC => "ENOEXEC",
            EBADF => "EBADF",
            ECHILD => "ECHILD",
            EAGAIN => "EAGAIN",
            ENOMEM => "ENOMEM",
            EACCES => "EACCES",
            EFAULT => "EFAULT",
            EBUSY => "EBUSY",
            EEXIST => "EEXIST",
            EXDEV => "EXDEV",
            ENODEV => "ENODEV",
            ENOTDIR => "ENOTDIR",
            EISDIR => "EISDIR",
            EINVAL => "EINVAL",
            ENFILE => "ENFILE",
            EMFILE => "EMFILE",
            ENOTTY => "ENOTTY",
            EFBIG => "EFBIG",
            ENOSPC => "ENOSPC",
            ESPIPE => "ESPIPE",
            EROFS => "EROFS",
            EMLINK => "EMLINK",
            EPIPE => "EPIPE",
            ERANGE => "ERANGE",
            EDEADLK => "EDEADLK",
            ENAMETOOLONG => "ENAMETOOLONG",
            ENOSYS => "ENOSYS",
            ENOTEMPTY => "ENOTEMPTY",
            ELOOP => "ELOOP",
            ENODATA => "ENODATA",
            EOVERFLOW => "EOVERFLOW",
            EILSEQ => "EILSEQ",
            ENOTSOCK => "ENOTSOCK",
            EDESTADDRREQ => "EDESTADDRREQ",
            EMSGSIZE => "EMSGSIZE",
            EPROTOTYPE => "EPROTOTYPE",
            ENOPROTOOPT => "ENOPROTOOPT",
            EPROTONOSUPPORT => "EPROTONOSUPPORT",
            EOPNOTSUPP => "EOPNOTSUPP",
            EAFNOSUPPORT => "EAFNOSUPPORT",
            EADDRINUSE => "EADDRINUSE",
            EADDRNOTAVAIL => "EADDRNOTAVAIL",
            ENETDOWN => "ENETDOWN",
            ENETUNREACH => "ENETUNREACH",
            ECONNABORTED => "ECONNABORTED",
            ECONNRESET => "ECONNRESET",
            ENOBUFS => "ENOBUFS",
            EISCONN => "EISCONN",
            ENOTCONN => "ENOTCONN",
            ESHUTDOWN => "ESHUTDOWN",
            ETIMEDOUT => "ETIMEDOUT",
            ECONNREFUSED => "ECONNREFUSED",
            EHOSTUNREACH => "EHOSTUNREACH",
            EALREADY => "EALREADY",
            EINPROGRESS => "EINPROGRESS",
            _ => "E?",
        }
    }
}

/// Droits portes par un handle.
pub mod rights {
    pub const SEND: u16 = 1 << 0;
    pub const RECV: u16 = 1 << 1;
    /// Le handle peut etre transmis a un autre processus (IPC, spawn).
    pub const GRANT: u16 = 1 << 2;
    pub const DUP: u16 = 1 << 3;
    pub const MAP: u16 = 1 << 4;
    pub const SIGNAL: u16 = 1 << 5;
    pub const WAIT: u16 = 1 << 6;
    pub const KILL: u16 = 1 << 7;
    /// Adresses physiques d'une memoire partagee (programmation DMA).
    pub const DMA: u16 = 1 << 8;
    /// Controle : reglages d'un thread, debogage d'un processus, revocation.
    pub const CONTROL: u16 = 1 << 9;
    /// Memoire partagee : projection en ecriture autorisee.
    pub const WRITE: u16 = 1 << 10;
    pub const ALL: u16 = 0x7FF;
}

/// Protections de `mem_map`.
pub mod prot {
    pub const READ: u64 = 1;
    pub const WRITE: u64 = 2;
    pub const EXEC: u64 = 4;
    /// Alloue les pages tout de suite (sinon : au premier acces, comme mmap).
    pub const POPULATE: u64 = 1 << 8;
}

/// Disposition de l'espace d'adressage utilisateur (VA sur 39 bits).
///
/// Tout ce qui est sous `USER_MIN` appartient au noyau (vue identite de la
/// memoire physique) et reste inaccessible depuis EL0.
pub mod layout {
    pub const PAGE_SIZE: u64 = 4096;
    /// Les 16 premiers GiB sont la vue identite du noyau (DRAM jusqu'a 14 GiB).
    pub const USER_MIN: u64 = 0x0000_0004_0000_0000; // 16 GiB
    pub const USER_MAX: u64 = 0x0000_0080_0000_0000; // 512 GiB
    pub const TEXT_BASE: u64 = 0x0000_0010_0000_0000; // code des programmes
    pub const HEAP_BASE: u64 = 0x0000_0020_0000_0000; // mem_map sans adresse
    pub const HEAP_END: u64 = 0x0000_0040_0000_0000;
    pub const MMIO_BASE: u64 = 0x0000_0040_0000_0000; // registres de peripheriques
    pub const MMIO_END: u64 = 0x0000_0060_0000_0000;
    pub const BOOTFS_BASE: u64 = 0x0000_0060_0000_0000; // image de demarrage (init)
    pub const UTCB_BASE: u64 = 0x0000_007E_0000_0000; // une page par thread
    pub const STACK_TOP: u64 = 0x0000_007F_F000_0000; // pile du thread principal
    pub const STACK_SIZE: u64 = 256 * 1024;
    pub const MAX_THREADS_PER_PROCESS: usize = 4096;
}

/// Etiquette de message : `[0:31]` label, `[32:41]` longueur en mots,
/// `[42:44]` nombre de handles, `[48:63]` drapeaux.
pub mod tag {
    /// Mots transportes dans les registres x2-x7.
    pub const REG_WORDS: usize = 6;
    /// Longueur maximale d'un message (registres + UTCB).
    pub const MAX_WORDS: usize = super::utcb::MR_WORDS;
    pub const MAX_HANDLES: usize = 4;
    /// Message synthetique : une notification liee au thread a ete signalee.
    pub const FLAG_NOTIFY: u64 = 1 << 48;

    #[inline(always)]
    pub const fn make(label: u32, words: usize, handles: usize) -> u64 {
        (label as u64) | ((words as u64 & 0x3FF) << 32) | ((handles as u64 & 0x7) << 42)
    }
    #[inline(always)]
    pub const fn label(t: u64) -> u32 {
        t as u32
    }
    #[inline(always)]
    pub const fn words(t: u64) -> usize {
        ((t >> 32) & 0x3FF) as usize
    }
    #[inline(always)]
    pub const fn handles(t: u64) -> usize {
        ((t >> 42) & 0x7) as usize
    }
    #[inline(always)]
    pub const fn flags(t: u64) -> u64 {
        t & (0xFFFF << 48)
    }
}

/// Page de controle propre a chaque thread (UTCB), projetee en lecture/ecriture
/// dans son processus et lue par le noyau via la memoire physique. Son adresse
/// est dans `TPIDRRO_EL0`, lisible depuis EL0 sans appel systeme.
pub mod utcb {
    /// Registres de message. Les 6 premiers voyagent dans x2-x7 ; le noyau ne
    /// recopie depuis la page que les mots 6 et suivants.
    pub const MR_WORDS: usize = 504;

    #[repr(C, align(4096))]
    pub struct Utcb {
        pub mr: [u64; MR_WORDS],
        /// Handles a transmettre (envoi) ou recus (reception).
        pub handles: [u32; super::tag::MAX_HANDLES],
        pub _reserved: [u64; 6],
    }

    const _: () = assert!(core::mem::size_of::<Utcb>() == 4096);
}

/// Arguments de `process_spawn`, passes par pointeur (comme `clone3`).
pub mod spawn {
    #[repr(C)]
    #[derive(Clone, Copy, Default)]
    pub struct SpawnArgs {
        pub elf_ptr: u64,
        pub elf_len: u64,
        pub name_ptr: u64,
        pub name_len: u64,
        /// Tableau de `HandleGrant` : handles copies dans le nouveau processus.
        pub grants_ptr: u64,
        pub grants_len: u64,
        /// Valeurs recues dans x0 / x1 par le point d'entree.
        pub arg0: u64,
        pub arg1: u64,
        /// 0 = priorite par defaut.
        pub prio: u32,
        /// -1 = choix du noyau.
        pub cpu: i32,
        /// Arguments puis environnement : `argc + envc` chaines terminees par
        /// NUL, a la suite (64 KiB au plus). Recopies dans le bloc de
        /// demarrage du nouveau processus.
        pub strings_ptr: u64,
        pub strings_len: u64,
        pub argc: u32,
        pub envc: u32,
        /// `SUSPENDED` : le thread principal attend un debogueur.
        pub flags: u64,
    }

    /// Demarrage suspendu : le premier thread s'arrete avant sa premiere
    /// instruction, jusqu'a l'attachement d'un debogueur.
    pub const SUSPENDED: u64 = 1 << 0;

    // Entrees et sorties d'un programme lance par le gestionnaire (drapeaux
    // de `proto::pm::SPAWN`). Un tube est transmis avec la requete (dans
    // l'ordre entree, sortie, erreurs) : lecteur (RECV) pour l'entree,
    // ecrivain (SEND) pour les sorties. Sinon : la console, ou rien (NULL).
    pub const STDIN_PIPE: u64 = 1 << 8;
    pub const STDOUT_PIPE: u64 = 1 << 9;
    pub const STDERR_PIPE: u64 = 1 << 10;
    pub const STDIN_NULL: u64 = 1 << 11;
    pub const STDOUT_NULL: u64 = 1 << 12;
    pub const STDERR_NULL: u64 = 1 << 13;

    #[repr(C)]
    #[derive(Clone, Copy, Default)]
    pub struct HandleGrant {
        pub handle: u32,
        pub slot: u32,
    }
}

/// Drapeaux de `thread_create` et reglages de `thread_set`.
pub mod thread {
    pub const DEFAULT_PRIO: u8 = 16;
    /// Plus basse priorite utilisable (0 = la plus haute, reservee au noyau).
    pub const MAX_PRIO: u8 = 30;
    /// thread_set : priorite (1..=30 ; 0 = defaut).
    pub const SET_PRIO: u64 = 0;
    /// thread_set : coeur impose (0..=3), ou `ANY_CPU`.
    pub const SET_AFFINITY: u64 = 1;
    pub const ANY_CPU: u64 = u64::MAX;
    /// `prio` 0 = defaut ; `cpu` None = choix du noyau.
    pub const fn flags(prio: u8, cpu: Option<u8>) -> u64 {
        let c = match cpu {
            Some(c) => c as u64 + 1,
            None => 0,
        };
        (prio as u64) | (c << 8)
    }
}

/// Emplacements des handles transmis au demarrage (convention d'amorcage).
pub mod slots {
    // init
    pub const INIT_IRQ_CONTROL: u32 = 1;
    pub const INIT_MMIO_ROOT: u32 = 2;
    /// Controle du systeme : heure murale.
    pub const INIT_SYSCTL: u32 = 3;
    // applications lancees par init
    /// Sortie standard (la console, ou un tube).
    pub const CONSOLE: u32 = 2;
    pub const PM: u32 = 3;
    /// Entree standard (`startup::HAS_STDIO`) : la console, ou un tube.
    pub const STDIN: u32 = 4;
    /// Sortie d'erreurs (`startup::HAS_STDIO`) : la console, ou un tube.
    pub const STDERR: u32 = 5;
    // pilote console (lance a part)
    pub const DRV_SERVICE: u32 = 2;
    pub const DRV_MMIO: u32 = 3;
    pub const DRV_IRQ: u32 = 4;
    /// Autres pilotes : console et gestionnaire comme une application, puis
    /// leurs ressources a partir d'ici : pages de registres, interruptions,
    /// controle du systeme, dans l'ordre de la table des pilotes d'init.
    pub const DEV_BASE: u32 = 8;
}

/// Demarrage d'un processus : x0 = drapeaux, x1 = argument libre du lanceur,
/// x2 = adresse du bloc de demarrage (en haut de la pile principale).
pub mod startup {
    pub const HAS_CONSOLE: u64 = 1 << 0;
    pub const HAS_PM: u64 = 1 << 1;
    /// Premier processus : x1 = taille de l'image de demarrage.
    pub const IS_INIT: u64 = 1 << 2;
    pub const IS_DRIVER: u64 = 1 << 3;
    /// Emplacements `slots::STDIN` et `slots::STDERR` fournis.
    pub const HAS_STDIO: u64 = 1 << 4;
    /// L'entree standard est un tube (lecture par `ipc_recv`), pas la console.
    pub const STDIN_PIPE: u64 = 1 << 5;
    /// Entree vide, sorties jetees.
    pub const STDIN_NULL: u64 = 1 << 6;
    pub const STDOUT_NULL: u64 = 1 << 7;
    pub const STDERR_NULL: u64 = 1 << 8;
    /// Les sorties sont des tubes : une erreur d'ecriture est un tube casse
    /// (pas de repli sur le journal du noyau, reserve a la console).
    pub const STDOUT_PIPE: u64 = 1 << 9;
    pub const STDERR_PIPE: u64 = 1 << 10;

    pub const BLOCK_MAGIC: u32 = u32::from_le_bytes(*b"NXSB");
    /// Plus grande zone de chaines (arguments + environnement).
    pub const MAX_STRINGS: u64 = 64 * 1024;

    /// Bloc de demarrage, l'equivalent de argv/envp/auxv sous Linux.
    #[repr(C)]
    #[derive(Clone, Copy, Default, Debug)]
    pub struct Block {
        pub magic: u32,
        pub version: u32,
        pub argc: u32,
        pub envc: u32,
        /// `argc` arguments puis `envc` variables « NOM=valeur », chacun
        /// termine par NUL.
        pub strings: u64,
        pub strings_len: u64,
        /// Copie des en-tetes de programme de l'executable (TLS, relro...).
        pub phdr: u64,
        pub phnum: u32,
        pub phent: u32,
        /// Base de chargement (0 : executable a adresse fixe).
        pub load_base: u64,
        pub entry: u64,
        /// Pile du thread principal.
        pub stack_base: u64,
        pub stack_size: u64,
        /// Graine aleatoire (comme AT_RANDOM).
        pub random: [u8; 16],
    }
}

/// Requetes de `sysinfo`.
pub mod info {
    pub const PROCESSES: u64 = 0;
    pub const MEMORY: u64 = 1;
    pub const CPUS: u64 = 2;

    pub const PSTATE_ALIVE: u32 = 1;
    pub const PSTATE_ZOMBIE: u32 = 2;

    #[repr(C)]
    #[derive(Clone, Copy, Default)]
    pub struct ProcInfo {
        pub pid: u32,
        pub threads: u32,
        pub pages: u64,
        pub name: [u8; 16],
        pub state: u32,
        pub parent: u32,
        /// Temps processeur consomme (ns).
        pub cpu_ns: u64,
    }

    #[repr(C)]
    #[derive(Clone, Copy, Default)]
    pub struct MemInfo {
        pub total_pages: u64,
        pub free_pages: u64,
        pub kernel_pages: u64,
    }

    #[repr(C)]
    #[derive(Clone, Copy, Default)]
    pub struct CpuInfo {
        pub online: u64,
        pub switches: u64,
        pub syscalls: u64,
        pub irqs: u64,
        pub idle_ns: u64,
        pub busy_ns: u64,
    }
}

/// Debogage : evenements envoyes au debogueur et reponses attendues.
///
/// Un thread d'un processus debogue qui fait une faute, execute `BRK`, finit
/// un pas a pas ou recoit un arret s'immobilise et « appelle » l'endpoint du
/// debogueur : etiquette `EVENT`, mots `[kind, tid, esr, far, pc, pid]`. La
/// reponse le relance : etiquette `RESUME` (mot 0 : `FLAG_STEP` pour une
/// seule instruction) ou `KILL`. Ses registres ne sont touches que par
/// `debug_regs`.
pub mod debug {
    pub const EVENT: u32 = 0x4442;
    pub const KIND_FAULT: u64 = 1;
    pub const KIND_BREAK: u64 = 2;
    pub const KIND_STEP: u64 = 3;
    pub const KIND_STOP: u64 = 4;

    pub const RESUME: u32 = 0;
    pub const KILL: u32 = 1;
    pub const FLAG_STEP: u64 = 1;

    /// Registres d'un thread (lecture/ecriture par `debug_regs`). Seuls les
    /// drapeaux NZCV de `pstate` sont modifiables.
    #[repr(C)]
    #[derive(Clone, Copy, Default, Debug)]
    pub struct Regs {
        pub x: [u64; 31],
        pub sp: u64,
        pub pc: u64,
        pub pstate: u64,
        pub tpidr: u64,
    }
}

/// Format de l'image de demarrage (bootfs) embarquee dans le noyau.
pub mod bootfs {
    pub const MAGIC: [u8; 8] = *b"NXKBOOT1";
    pub const NAME_LEN: usize = 24;

    #[repr(C)]
    #[derive(Clone, Copy)]
    pub struct Header {
        pub magic: [u8; 8],
        pub count: u32,
        pub _reserved: u32,
    }

    #[repr(C)]
    #[derive(Clone, Copy)]
    pub struct Entry {
        pub name: [u8; NAME_LEN],
        pub offset: u32,
        pub size: u32,
    }
}

/// Protocoles des services systeme (labels de message).
///
/// Sauf mention contraire, une reponse porte le code d'erreur (errno
/// positif, 0 = succes) dans son etiquette.
pub mod proto {
    pub mod console {
        /// Ecriture : mot 0 = nombre d'octets, octets a partir du mot 1.
        pub const WRITE: u32 = 1;
        /// Lecture d'une ligne : reponse au meme format que WRITE.
        pub const READ_LINE: u32 = 2;
    }
    pub mod pm {
        /// Lancement : octets = argc (u32), envc (u32), drapeaux (u32,
        /// `spawn::SUSPENDED`...), 0 (u32), puis `argc + envc` chaines
        /// terminees par NUL (le premier argument est le programme).
        /// Reponse : handle du processus transmis.
        pub const SPAWN: u32 = 1;
        /// Liste des programmes disponibles, noms separes par des espaces.
        pub const LIST: u32 = 2;
        /// Publication d'un service : octets = nom, handle 0 = endpoint
        /// (droit SEND) a remettre aux clients. Reserve aux services lances
        /// par init.
        pub const REGISTER: u32 = 3;
        /// Recherche d'un service : octets = nom. Reponse : handle transmis
        /// (ENOENT si le service n'est pas publie).
        pub const LOOKUP: u32 = 4;
        /// Comme LOOKUP, mais attend que le service soit publie.
        pub const LOOKUP_WAIT: u32 = 5;
        /// Services publies, noms separes par des espaces.
        pub const SERVICES: u32 = 6;
        /// Insignes de l'endpoint du gestionnaire : applications, services.
        pub const BADGE_APP: u64 = 1;
        pub const BADGE_SERVICE: u64 = 2;
    }
    /// Horloges et resets des peripheriques (CAR).
    pub mod clk {
        /// mot 0 = peripherique : active son horloge et le sort du reset.
        /// Un bloc dont une partition d'alimentation (PMC) est coupee n'est
        /// pas touche : ENODEV (clk n'allume pas les partitions). Un bloc de
        /// partition deja pret est laisse tel quel. EIO si l'etat lu ensuite
        /// n'est pas `ST_READY`.
        pub const ENABLE: u32 = 1;
        pub const DISABLE: u32 = 2;
        /// mot 0 = peripherique. Reponse : mot 0 = etat (bits `ST_*`). Sur
        /// Tegra, acceder a un bloc sans horloge, en reset ou hors tension
        /// bloque le bus : un pilote ne touche a ses registres qu'avec
        /// `ST_READY` complet.
        pub const STATUS: u32 = 3;
        /// Toutes ses horloges actives.
        pub const ST_CLOCK: u64 = 1;
        /// Sorti du reset.
        pub const ST_OUT_OF_RESET: u64 = 2;
        /// Partitions d'alimentation allumees et sans verrou (clamp).
        pub const ST_POWERED: u64 = 4;
        pub const ST_READY: u64 = ST_CLOCK | ST_OUT_OF_RESET | ST_POWERED;
        pub const I2C1: u64 = 1;
        pub const I2C2: u64 = 2;
        pub const I2C3: u64 = 3;
        pub const I2C4: u64 = 4;
        pub const I2C5: u64 = 5;
        pub const I2C6: u64 = 6;
        pub const SDMMC1: u64 = 10;
        pub const SDMMC4: u64 = 13;
        /// Hote xHCI : horloges de l'hote et de la logique SuperSpeed,
        /// partitions XUSBA et XUSBC.
        pub const XUSB_HOST: u64 = 20;
    }
    /// Bus I2C.
    pub mod i2c {
        /// mot 0 = bus (1..6) | adresse << 8 | octets a lire << 16 ; octets
        /// a ecrire ensuite (format `pack`). Reponse : octets lus.
        pub const TRANSFER: u32 = 1;
        pub const fn header(bus: u64, addr: u64, read: u64) -> u64 {
            bus | addr << 8 | read << 16
        }
    }
    /// Alimentation, heure, capteurs.
    pub mod power {
        pub const OFF: u32 = 1;
        pub const REBOOT: u32 = 2;
        /// Reponse : mot 0 = temperature locale, mot 1 = distante (m degC).
        pub const TEMPERATURE: u32 = 3;
        /// Relit la RTC et remet l'heure murale du noyau a jour.
        pub const SYNC_TIME: u32 = 4;
    }
    /// Peripherique bloc (carte SD, eMMC).
    pub mod blk {
        /// Reponse : mot 0 = taille de bloc, mot 1 = nombre de blocs.
        pub const INFO: u32 = 1;
        /// handle 0 = memoire partagee servant de tampon d'echange.
        pub const ATTACH: u32 = 2;
        /// mots : bloc de depart, nombre de blocs, decalage dans le tampon.
        pub const READ: u32 = 3;
        pub const WRITE: u32 = 4;
    }
    /// Systeme de fichiers.
    pub mod fs {
        /// octets = chemin ; mot place avant : drapeaux (`O_*`). Reponse :
        /// mot 0 = descripteur, mot 1 = taille.
        pub const OPEN: u32 = 1;
        /// mots : descripteur, position, longueur (<= MAX_IO). Reponse :
        /// octets lus.
        pub const READ: u32 = 2;
        /// mots : descripteur, position ; octets a ecrire.
        pub const WRITE: u32 = 3;
        pub const CLOSE: u32 = 4;
        /// octets = chemin. Reponse : mot 0 = taille, mot 1 = genre, mot 2 =
        /// date de modification (s depuis 1970).
        pub const STAT: u32 = 5;
        /// mots : index ; octets = chemin du dossier. Reponse : mot 0 =
        /// taille, mot 1 = genre, octets = nom (vide : fin du dossier).
        pub const READDIR: u32 = 6;
        pub const MKDIR: u32 = 7;
        pub const UNLINK: u32 = 8;
        pub const RMDIR: u32 = 9;
        /// octets = « ancien\0nouveau ».
        pub const RENAME: u32 = 10;
        /// mots : descripteur, nouvelle taille.
        pub const TRUNCATE: u32 = 11;
        /// Ecrit sur le support tout ce qui est en cache.
        pub const SYNC: u32 = 12;
        /// Reponse : mot 0 = octets libres, mot 1 = octets totaux.
        pub const STATFS: u32 = 13;
        /// mots : descripteur. Reponse comme STAT (fichier ouvert).
        pub const FSTAT: u32 = 14;
        /// mots : descripteur. Reference supplementaire au meme fichier
        /// ouvert ; CLOSE ne le ferme qu'a la derniere.
        pub const DUP: u32 = 15;

        pub const O_READ: u64 = 1;
        pub const O_WRITE: u64 = 2;
        pub const O_CREATE: u64 = 4;
        pub const O_TRUNC: u64 = 8;
        pub const O_EXCL: u64 = 16;

        pub const KIND_FILE: u64 = 1;
        pub const KIND_DIR: u64 = 2;
        /// Octets par lecture ou ecriture en un message.
        pub const MAX_IO: usize = 3 * 1024;
    }
    /// Affichage.
    pub mod display {
        /// Reponse : mots = largeur, hauteur, pas (octets par ligne), format
        /// (1 = A8B8G8R8) ; handle 0 = memoire de l'image.
        pub const OPEN: u32 = 1;
        /// Texte a afficher dans le terminal de l'ecran (format `pack`).
        pub const WRITE: u32 = 2;
        pub const FORMAT_RGBA8888: u64 = 1;
    }
    /// Interface Ethernet (couche liaison).
    pub mod eth {
        /// Reponse : mot 0 = adresse MAC (6 octets de poids faible, octet 0
        /// en premier), mot 1 = MTU.
        pub const INFO: u32 = 1;
        /// Trame a emettre (format `pack`).
        pub const SEND: u32 = 2;
        /// Attend une trame (mot 0 = delai en ns, 0 : sans limite).
        pub const RECV: u32 = 3;
    }
    /// Pile TCP/IP (IPv4). Adresses sur 32 bits, `a.b.c.d` = `a << 24 | ...`
    /// (comme `u32::from(Ipv4Addr)`). Les delais sont en ns (0 : sans limite).
    pub mod net {
        /// mots : adresse, port, delai. Reponse : mot 0 = socket.
        pub const TCP_CONNECT: u32 = 1;
        /// mots : port, file d'attente. Reponse : mot 0 = socket d'ecoute.
        pub const TCP_LISTEN: u32 = 2;
        /// mots : socket d'ecoute, delai. Reponse : socket, adresse, port.
        pub const TCP_ACCEPT: u32 = 3;
        /// mots : socket, delai ; octets. Reponse : mot 0 = octets acceptes.
        pub const SEND: u32 = 4;
        /// mots : socket, longueur maximale, delai. Reponse : octets (aucun :
        /// fin de flux).
        pub const RECV: u32 = 5;
        /// mots : socket.
        pub const CLOSE: u32 = 6;
        /// mots : socket. Fin d'emission (FIN), la reception continue.
        pub const SHUTDOWN: u32 = 7;
        /// mots : port (0 : ephemere). Reponse : mot 0 = socket UDP.
        pub const UDP_BIND: u32 = 8;
        /// mots : socket, adresse, port ; octets.
        pub const SEND_TO: u32 = 9;
        /// mots : socket, delai. Reponse : mots adresse, port ; octets.
        pub const RECV_FROM: u32 = 10;
        /// octets = nom d'hote. Reponse : mot 0 = adresse.
        pub const RESOLVE: u32 = 11;
        /// Reponse : adresse, longueur de prefixe, passerelle, serveur DNS.
        pub const INFO: u32 = 12;
        /// mots : socket. Reponse : adresse et port locaux, puis distants.
        pub const ADDRS: u32 = 13;
        /// mots : socket. Reference supplementaire au meme socket (meme
        /// identifiant) ; CLOSE ne le ferme qu'a la derniere.
        pub const DUP: u32 = 14;
    }
    /// Evenements d'entree.
    pub mod input {
        /// Attend le prochain evenement. Reponse : mot 0 = genre, mot 1 =
        /// code, mot 2 = valeur, mots 3-4 = x, y. Mot 0 de la requete : delai
        /// d'attente en ns (0 : sans limite).
        pub const READ: u32 = 1;
        /// Evenement fourni par un autre pilote (clavier USB...) : memes mots.
        pub const INJECT: u32 = 2;
        pub const KIND_KEY: u64 = 1;
        pub const KIND_TOUCH: u64 = 2;
        pub const KIND_BUTTON: u64 = 3;
        /// Toucher : code = appui (1), mouvement (2), relachement (3) ;
        /// valeur = numero du doigt.
        pub const TOUCH_DOWN: u64 = 1;
        pub const TOUCH_MOVE: u64 = 2;
        pub const TOUCH_UP: u64 = 3;
        /// Touche : code = code HID (page clavier), valeur = 1 appui, 0
        /// relachement ; x = caractere Unicode produit (0 : aucun).
        pub const KEY_PRESSED: u64 = 1;
    }
}
