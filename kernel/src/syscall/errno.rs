/// Error codes used by system calls
/// These follow the Linux errno conventions

pub const EPERM: isize = 1;         /* Operation not permitted */
pub const ENOENT: isize = 2;        /* No such file or directory */
pub const ESRCH: isize = 3;         /* No such process */
pub const EINTR: isize = 4;         /* Interrupted system call */
pub const EIO: isize = 5;           /* I/O error */
pub const ENXIO: isize = 6;         /* No such device or address */
pub const E2BIG: isize = 7;         /* Argument list too long */
pub const ENOEXEC: isize = 8;       /* Exec format error */
pub const EBADF: isize = 9;         /* Bad file number */
pub const ECHILD: isize = 10;       /* No child processes */
pub const EAGAIN: isize = 11;       /* Try again */
pub const ENOMEM: isize = 12;       /* Out of memory */
pub const EACCES: isize = 13;       /* Permission denied */
pub const EFAULT: isize = 14;       /* Bad address */
pub const ENOTBLK: isize = 15;      /* Block device required */
pub const EBUSY: isize = 16;        /* Device or resource busy */
pub const EEXIST: isize = 17;       /* File exists */
pub const EXDEV: isize = 18;        /* Cross-device link */
pub const ENODEV: isize = 19;       /* No such device */
pub const ENOTDIR: isize = 20;      /* Not a directory */
pub const EISDIR: isize = 21;       /* Is a directory */
pub const EINVAL: isize = 22;       /* Invalid argument */
pub const ENFILE: isize = 23;       /* File table overflow */
pub const EMFILE: isize = 24;       /* Too many open files */
pub const ENOTTY: isize = 25;       /* Not a typewriter */
pub const ETXTBSY: isize = 26;      /* Text file busy */
pub const EFBIG: isize = 27;        /* File too large */
pub const ENOSPC: isize = 28;       /* No space left on device */
pub const ESPIPE: isize = 29;       /* Illegal seek */
pub const EROFS: isize = 30;        /* Read-only file system */
pub const EMLINK: isize = 31;       /* Too many links */
pub const EPIPE: isize = 32;        /* Broken pipe */

/* Math software */
pub const EDOM: isize = 33;         /* Math argument out of domain of func */
pub const ERANGE: isize = 34;       /* Math result not representable */

/* File locking */
pub const EDEADLK: isize = 35;      /* Resource deadlock would occur */
pub const ENAMETOOLONG: isize = 36; /* File name too long */
pub const ENOLCK: isize = 37;       /* No record locks available */
pub const ENOSYS: isize = 38;       /* Function not implemented */
pub const ENOTEMPTY: isize = 39;    /* Directory not empty */
pub const ELOOP: isize = 40;        /* Too many symbolic links encountered */

pub const ENOMSG: isize = 42;       /* No message of desired type */
pub const EIDRM: isize = 43;        /* Identifier removed */
pub const ECHRNG: isize = 44;       /* Channel number out of range */
pub const EL2NSYNC: isize = 45;     /* Level 2 not synchronized */
pub const EL3HLT: isize = 46;       /* Level 3 halted */
pub const EL3RST: isize = 47;       /* Level 3 reset */
pub const ELNRNG: isize = 48;       /* Link number out of range */
pub const EUNATCH: isize = 49;      /* Protocol driver not attached */
pub const ENOCSI: isize = 50;       /* No CSI structure available */
pub const EL2HLT: isize = 51;       /* Level 2 halted */
pub const EBADE: isize = 52;        /* Invalid exchange */
pub const EBADR: isize = 53;        /* Invalid request descriptor */
pub const EXFULL: isize = 54;       /* Exchange full */
pub const ENOANO: isize = 55;       /* No anode */
pub const EBADRQC: isize = 56;      /* Invalid request code */
pub const EBADSLT: isize = 57;      /* Invalid slot */

pub const EDEADLOCK: isize = EDEADLK;

pub const EBFONT: isize = 59;        /* Bad font file format */
pub const ENOSTR: isize = 60;       /* Device not a stream */
pub const ENODATA: isize = 61;      /* No data available */
pub const ETIME: isize = 62;        /* Timer expired */
pub const ENOSR: isize = 63;        /* Out of streams resources */
pub const ENONET: isize = 64;       /* Machine is not on the network */
pub const ENOPKG: isize = 65;       /* Package not installed */
pub const EREMOTE: isize = 66;      /* Object is remote */
pub const ENOLINK: isize = 67;      /* Link has been severed */
pub const EADV: isize = 68;         /* Advertise error */
pub const ESRMNT: isize = 69;       /* Srmount error */
pub const ECOMM: isize = 70;        /* Communication error on send */
pub const EPROTO: isize = 71;       /* Protocol error */
pub const EMULTIHOP: isize = 72;    /* Multihop attempted */
pub const EDOTDOT: isize = 73;      /* RFS specific error */
pub const EBADMSG: isize = 74;      /* Not a data message */
pub const EOVERFLOW: isize = 75;    /* Value too large for defined data type */
pub const ENOTUNIQ: isize = 76;     /* Name not unique on network */
pub const EBADFD: isize = 77;       /* File descriptor in bad state */
pub const EREMCHG: isize = 78;      /* Remote address changed */
pub const ELIBACC: isize = 79;      /* Can not access a needed shared library */
pub const ELIBBAD: isize = 80;      /* Accessing a corrupted shared library */
pub const ELIBSCN: isize = 81;      /* .lib section in a.out corrupted */
pub const ELIBMAX: isize = 82;      /* Attempting to link in too many shared libraries */
pub const ELIBEXEC: isize = 83;     /* Cannot exec a shared library directly */
pub const EILSEQ: isize = 84;       /* Illegal byte sequence */
pub const ERESTART: isize = 85;     /* Interrupted system call should be restarted */
pub const ESTRPIPE: isize = 86;     /* Streams pipe error */
pub const EUSERS: isize = 87;       /* Too many users */
pub const ENOTSOCK: isize = 88;     /* Socket operation on non-socket */
pub const EDESTADDRREQ: isize = 89; /* Destination address required */
pub const EMSGSIZE: isize = 90;     /* Message too long */
pub const EPROTOTYPE: isize = 91;   /* Protocol wrong type for socket */
pub const ENOPROTOOPT: isize = 92;  /* Protocol not available */
pub const EPROTONOSUPPORT: isize = 93; /* Protocol not supported */
pub const ESOCKTNOSUPPORT: isize = 94; /* Socket type not supported */
pub const EOPNOTSUPP: isize = 95;   /* Operation not supported on transport endpoint */
pub const EPFNOSUPPORT: isize = 96; /* Protocol family not supported */
pub const EAFNOSUPPORT: isize = 97; /* Address family not supported by protocol */
pub const EADDRINUSE: isize = 98;   /* Address already in use */
pub const EADDRNOTAVAIL: isize = 99; /* Cannot assign requested address */
pub const ENETDOWN: isize = 100;    /* Network is down */
pub const ENETUNREACH: isize = 101; /* Network is unreachable */
pub const ENETRESET: isize = 102;   /* Network dropped connection because of reset */
pub const ECONNABORTED: isize = 103; /* Software caused connection abort */
pub const ECONNRESET: isize = 104;  /* Connection reset by peer */
pub const ENOBUFS: isize = 105;     /* No buffer space available */
pub const EISCONN: isize = 106;     /* Transport endpoint is already connected */
pub const ENOTCONN: isize = 107;    /* Transport endpoint is not connected */
pub const ESHUTDOWN: isize = 108;   /* Cannot send after transport endpoint shutdown */
pub const ETOOMANYREFS: isize = 109; /* Too many references: cannot splice */
pub const ETIMEDOUT: isize = 110;   /* Connection timed out */
pub const ECONNREFUSED: isize = 111; /* Connection refused */
pub const EHOSTDOWN: isize = 112;   /* Host is down */
pub const EHOSTUNREACH: isize = 113; /* No route to host */
pub const EALREADY: isize = 114;    /* Operation already in progress */
pub const EINPROGRESS: isize = 115; /* Operation now in progress */
pub const ESTALE: isize = 116;      /* Stale file handle */
pub const EUCLEAN: isize = 117;     /* Structure needs cleaning */
pub const ENOTNAM: isize = 118;     /* Not a XENIX named type file */
pub const ENAVAIL: isize = 119;     /* No XENIX semaphores available */
pub const EISNAM: isize = 120;      /* Is a named type file */
pub const EREMOTEIO: isize = 121;   /* Remote I/O error */
pub const EDQUOT: isize = 122;      /* Quota exceeded */
pub const ENOMEDIUM: isize = 123;   /* No medium found */
pub const EMEDIUMTYPE: isize = 124; /* Wrong medium type */
pub const ECANCELED: isize = 125;   /* Operation Canceled */
pub const ENOKEY: isize = 126;      /* Required key not available */
pub const EKEYEXPIRED: isize = 127; /* Key has expired */
pub const EKEYREVOKED: isize = 128; /* Key has been revoked */
pub const EKEYREJECTED: isize = 129; /* Key was rejected by service */

/* for robust mutexes */
pub const EOWNERDEAD: isize = 130;  /* Owner died */
pub const ENOTRECOVERABLE: isize = 131; /* State not recoverable */

pub const ERFKILL: isize = 132;     /* Operation not possible due to RF-kill */
pub const EHWPOISON: isize = 133;   /* Memory page has hardware error */

/* Aliases for commonly used error codes */
pub const EWOULDBLOCK: isize = EAGAIN;
pub const ENOTSUP: isize = EOPNOTSUPP;