//! Codes d'erreur : ceux de Linux, rendus en negatif par le noyau et par les
//! services (fichiers, reseau). Pas d'errno par thread : chaque appel rend
//! son erreur.

use crate::io::ErrorKind;
use crate::sys::pal::abi::err::*;

pub fn errno() -> i32 {
    0
}

pub fn is_interrupted(code: i32) -> bool {
    code as i64 == EINTR
}

pub fn decode_error_kind(code: i32) -> ErrorKind {
    use ErrorKind::*;
    match code as i64 {
        E2BIG => ArgumentListTooLong,
        EADDRINUSE => AddrInUse,
        EADDRNOTAVAIL => AddrNotAvailable,
        EBUSY => ResourceBusy,
        ECONNABORTED => ConnectionAborted,
        ECONNREFUSED => ConnectionRefused,
        ECONNRESET => ConnectionReset,
        EDEADLK => Deadlock,
        ENOTEMPTY => DirectoryNotEmpty,
        EEXIST => AlreadyExists,
        EFBIG => FileTooLarge,
        EHOSTUNREACH => HostUnreachable,
        EINTR => Interrupted,
        EINVAL => InvalidInput,
        EISDIR => IsADirectory,
        ELOOP => FilesystemLoop,
        ENOENT => NotFound,
        ENOMEM => OutOfMemory,
        ENOSPC => StorageFull,
        ENOSYS | EOPNOTSUPP | EAFNOSUPPORT | EPROTONOSUPPORT => Unsupported,
        EMLINK => TooManyLinks,
        ENAMETOOLONG => InvalidFilename,
        ENETDOWN => NetworkDown,
        ENETUNREACH => NetworkUnreachable,
        ENOTCONN => NotConnected,
        ENOTDIR => NotADirectory,
        EPIPE | ESHUTDOWN => BrokenPipe,
        EROFS => ReadOnlyFilesystem,
        ESPIPE => NotSeekable,
        ETIMEDOUT => TimedOut,
        EXDEV => CrossesDevices,
        EINPROGRESS => InProgress,
        EACCES | EPERM => PermissionDenied,
        EAGAIN => WouldBlock,
        EILSEQ => InvalidData,
        _ => Uncategorized,
    }
}

pub fn error_string(code: i32) -> String {
    let s = match code as i64 {
        EPERM => "operation non permise",
        ENOENT => "fichier ou dossier introuvable",
        ESRCH => "processus introuvable",
        EINTR => "appel interrompu",
        EIO => "erreur d'entree/sortie",
        ENXIO => "peripherique ou adresse introuvable",
        E2BIG => "liste d'arguments trop longue",
        ENOEXEC => "format d'executable invalide",
        EBADF => "handle invalide",
        ECHILD => "aucun processus enfant",
        EAGAIN => "ressource temporairement indisponible",
        ENOMEM => "memoire insuffisante",
        EACCES => "permission refusee",
        EFAULT => "adresse invalide",
        EBUSY => "ressource occupee",
        EEXIST => "le fichier existe deja",
        EXDEV => "lien entre peripheriques",
        ENODEV => "peripherique introuvable",
        ENOTDIR => "n'est pas un dossier",
        EISDIR => "est un dossier",
        EINVAL => "argument invalide",
        ENFILE | EMFILE => "trop de fichiers ouverts",
        ENOTTY => "operation inadaptee au peripherique",
        EFBIG => "fichier trop gros",
        ENOSPC => "plus de place sur le support",
        ESPIPE => "deplacement impossible",
        EROFS => "systeme de fichiers en lecture seule",
        EMLINK => "trop de liens",
        EPIPE => "tube casse",
        ERANGE => "resultat hors limites",
        EDEADLK => "interblocage evite",
        ENAMETOOLONG => "nom trop long",
        ENOSYS => "fonction non implementee",
        ENOTEMPTY => "dossier non vide",
        ELOOP => "trop de niveaux de liens",
        ENODATA => "aucune donnee",
        EOVERFLOW => "valeur trop grande",
        EILSEQ => "sequence d'octets invalide",
        ENOTSOCK => "n'est pas un socket",
        EDESTADDRREQ => "adresse de destination requise",
        EMSGSIZE => "message trop long",
        EPROTOTYPE | ENOPROTOOPT | EPROTONOSUPPORT => "protocole non disponible",
        EOPNOTSUPP => "operation non supportee",
        EAFNOSUPPORT => "famille d'adresses non supportee",
        EADDRINUSE => "adresse deja utilisee",
        EADDRNOTAVAIL => "adresse indisponible",
        ENETDOWN => "reseau hors service",
        ENETUNREACH => "reseau inaccessible",
        ECONNABORTED => "connexion abandonnee",
        ECONNRESET => "connexion reinitialisee par le correspondant",
        ENOBUFS => "plus de tampons disponibles",
        EISCONN => "deja connecte",
        ENOTCONN => "non connecte",
        ESHUTDOWN => "emission terminee",
        ETIMEDOUT => "delai depasse",
        ECONNREFUSED => "connexion refusee",
        EHOSTUNREACH => "hote inaccessible",
        EALREADY => "operation deja en cours",
        EINPROGRESS => "operation en cours",
        _ => "erreur inconnue",
    };
    s.to_string()
}
