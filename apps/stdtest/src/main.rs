//! Tests de la bibliotheque standard sur nxk : un programme Rust ordinaire,
//! sans rien de propre au systeme, qui exerce threads, synchronisation,
//! canaux, temps, memoire, environnement, fichiers, processus et reseau.
//!
//!     stdtest            tous les tests (fichiers : carte SD requise)
//!     stdtest net        plus le reseau (acces a Internet requis)
//!     stdtest bench      mesures de quelques operations courantes

use std::alloc::{Layout, alloc, dealloc};
use std::cell::Cell;
use std::collections::{BTreeMap, HashMap, VecDeque};
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::net::{TcpStream, ToSocketAddrs, UdpSocket};
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Barrier, Condvar, Mutex, OnceLock, RwLock, mpsc};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use std::{env, fs, hint, thread};

type R = Result<String, String>;

fn ensure(c: bool, msg: impl Into<String>) -> Result<(), String> {
    if c { Ok(()) } else { Err(msg.into()) }
}

fn threads() -> R {
    let handles: Vec<_> = (0..16u64).map(|k| thread::spawn(move || (k * 1000..(k + 1) * 1000).sum::<u64>())).collect();
    let total: u64 = handles.into_iter().map(|h| h.join().unwrap()).sum();
    ensure(total == (0..16000u64).sum(), format!("somme {total}"))?;
    let named = thread::Builder::new().name("ouvrier".into()).spawn(|| thread::current().name().map(String::from)).unwrap();
    ensure(named.join().unwrap().as_deref() == Some("ouvrier"), "nom du thread")?;
    let cpus = thread::available_parallelism().map(|n| n.get()).unwrap_or(0);
    Ok(format!("16 threads, {cpus} coeurs"))
}

fn scope() -> R {
    let data: Vec<u32> = (1..=1000).collect();
    let (a, b) = data.split_at(500);
    let (sa, sb) = thread::scope(|s| {
        let ha = s.spawn(|| a.iter().sum::<u32>());
        let hb = s.spawn(|| b.iter().sum::<u32>());
        (ha.join().unwrap(), hb.join().unwrap())
    });
    ensure(sa + sb == 500500, "somme")?;
    Ok("emprunts entre threads".into())
}

fn mutex() -> R {
    let m = Arc::new(Mutex::new(0u64));
    let hs: Vec<_> = (0..8)
        .map(|_| {
            let m = m.clone();
            thread::spawn(move || {
                for _ in 0..10_000 {
                    *m.lock().unwrap() += 1;
                }
            })
        })
        .collect();
    for h in hs {
        h.join().unwrap();
    }
    let v = *m.lock().unwrap();
    ensure(v == 80_000, format!("compteur {v}"))?;
    Ok("8 x 10000 sous contention".into())
}

fn condvar() -> R {
    let q = Arc::new((Mutex::new(VecDeque::new()), Condvar::new()));
    let prod = {
        let q = q.clone();
        thread::spawn(move || {
            for i in 0..1000u32 {
                q.0.lock().unwrap().push_back(i);
                q.1.notify_one();
            }
        })
    };
    let mut got = 0u64;
    let mut n = 0;
    while n < 1000 {
        let mut g = q.0.lock().unwrap();
        while g.is_empty() {
            g = q.1.wait(g).unwrap();
        }
        while let Some(v) = g.pop_front() {
            got += v as u64;
            n += 1;
        }
    }
    prod.join().unwrap();
    ensure(got == 499_500, "somme")?;
    let (m, c) = (Mutex::new(()), Condvar::new());
    let t0 = Instant::now();
    let (_g, r) = c.wait_timeout(m.lock().unwrap(), Duration::from_millis(20)).unwrap();
    ensure(r.timed_out() && t0.elapsed() >= Duration::from_millis(20), "attente bornee")?;
    Ok("producteur/consommateur, delai".into())
}

fn rwlock() -> R {
    let l = Arc::new(RwLock::new(vec![0u32; 16]));
    let w = {
        let l = l.clone();
        thread::spawn(move || {
            for i in 0..500 {
                let mut g = l.write().unwrap();
                for x in g.iter_mut() {
                    *x = i;
                }
            }
        })
    };
    let readers: Vec<_> = (0..4)
        .map(|_| {
            let l = l.clone();
            thread::spawn(move || {
                for _ in 0..500 {
                    let g = l.read().unwrap();
                    // Un ecrivain ne doit jamais etre vu a moitie.
                    if g.iter().any(|&x| x != g[0]) {
                        return false;
                    }
                }
                true
            })
        })
        .collect();
    w.join().unwrap();
    for r in readers {
        ensure(r.join().unwrap(), "lecture incoherente")?;
    }
    Ok("1 ecrivain, 4 lecteurs".into())
}

fn channels() -> R {
    let (tx, rx) = mpsc::channel::<u32>();
    for p in 0..4 {
        let tx = tx.clone();
        thread::spawn(move || {
            for i in 0..1000 {
                tx.send(p * 1000 + i).unwrap();
            }
        });
    }
    drop(tx);
    let sum: u64 = rx.iter().map(|v| v as u64).sum();
    ensure(sum == (0..4000u64).sum(), "somme")?;
    // Rendez-vous : canal synchrone sans tampon.
    let (tx, rx) = mpsc::sync_channel::<u32>(0);
    let h = thread::spawn(move || (0..100).map(|_| rx.recv().unwrap()).sum::<u32>());
    for i in 0..100 {
        tx.send(i).unwrap();
    }
    ensure(h.join().unwrap() == 4950, "rendez-vous")?;
    let (_tx, rx) = mpsc::channel::<u32>();
    ensure(rx.recv_timeout(Duration::from_millis(10)).is_err(), "recv_timeout")?;
    Ok("4 producteurs, rendez-vous, delai".into())
}

fn barrier_once() -> R {
    let b = Arc::new(Barrier::new(8));
    let inits = Arc::new(AtomicUsize::new(0));
    static CELL: OnceLock<u32> = OnceLock::new();
    let hs: Vec<_> = (0..8)
        .map(|k| {
            let (b, inits) = (b.clone(), inits.clone());
            thread::spawn(move || {
                b.wait();
                *CELL.get_or_init(|| {
                    inits.fetch_add(1, Ordering::SeqCst);
                    k
                })
            })
        })
        .collect();
    let vals: Vec<u32> = hs.into_iter().map(|h| h.join().unwrap()).collect();
    ensure(inits.load(Ordering::SeqCst) == 1, "initialisations multiples")?;
    ensure(vals.iter().all(|&v| v == vals[0]), "valeurs differentes")?;
    Ok("Barrier(8), OnceLock".into())
}

static DROPS: AtomicUsize = AtomicUsize::new(0);

struct Tracker(Cell<u32>);

impl Drop for Tracker {
    fn drop(&mut self) {
        DROPS.fetch_add(1, Ordering::SeqCst);
    }
}

thread_local! {
    static LOCAL: Tracker = const { Tracker(Cell::new(0)) };
    static COUNTER: Cell<u64> = const { Cell::new(0) };
}

fn thread_locals() -> R {
    let before = DROPS.load(Ordering::SeqCst);
    let hs: Vec<_> = (0..6u32)
        .map(|k| {
            thread::spawn(move || {
                LOCAL.with(|t| t.0.set(k));
                for _ in 0..100 {
                    COUNTER.with(|c| c.set(c.get() + 1));
                }
                (LOCAL.with(|t| t.0.get()), COUNTER.with(|c| c.get()))
            })
        })
        .collect();
    for (k, h) in hs.into_iter().enumerate() {
        let (v, c) = h.join().unwrap();
        ensure(v == k as u32 && c == 100, format!("thread {k} : {v} {c}"))?;
    }
    let d = DROPS.load(Ordering::SeqCst) - before;
    ensure(d == 6, format!("{d} destructeurs sur 6"))?;
    Ok("valeurs propres, 6 destructeurs".into())
}

fn park() -> R {
    let t0 = Instant::now();
    thread::park_timeout(Duration::from_millis(15));
    ensure(t0.elapsed() >= Duration::from_millis(15), "park_timeout trop court")?;
    let main = thread::current();
    let flag = Arc::new(AtomicUsize::new(0));
    let f = flag.clone();
    let h = thread::spawn(move || {
        thread::sleep(Duration::from_millis(5));
        f.store(1, Ordering::SeqCst);
        main.unpark();
    });
    while flag.load(Ordering::SeqCst) == 0 {
        thread::park();
    }
    h.join().unwrap();
    Ok("park, unpark, park_timeout".into())
}

fn sleep_time() -> R {
    let t0 = Instant::now();
    thread::sleep(Duration::from_millis(50));
    let dt = t0.elapsed();
    ensure(dt >= Duration::from_millis(50) && dt < Duration::from_millis(150), format!("{dt:?}"))?;
    let now = SystemTime::now().duration_since(UNIX_EPOCH).map_err(|e| e.to_string())?;
    let year = 1970 + now.as_secs() / 31_556_952;
    Ok(format!("sleep(50 ms) = {:.1} ms, annee ~{year}", dt.as_secs_f64() * 1000.0))
}

fn environment() -> R {
    let args: Vec<String> = env::args().collect();
    ensure(args.first().map(|a| a.ends_with("stdtest")).unwrap_or(false), format!("args {args:?}"))?;
    unsafe { env::set_var("NXK_ESSAI", "valeur=1") };
    ensure(env::var("NXK_ESSAI").as_deref() == Ok("valeur=1"), "set_var")?;
    ensure(env::vars().any(|(k, _)| k == "NXK_ESSAI"), "vars")?;
    unsafe { env::remove_var("NXK_ESSAI") };
    ensure(env::var("NXK_ESSAI").is_err(), "remove_var")?;
    ensure(env::consts::OS == "nxk", env::consts::OS)?;
    {
        use std::io::IsTerminal;
        ensure(std::io::stdout().is_terminal(), "stdout devrait etre la console")?;
    }
    let cwd = env::current_dir().map_err(|e| e.to_string())?;
    Ok(format!("{} arguments, dossier {}", args.len(), cwd.display()))
}

fn memory() -> R {
    // Gros tableau (pages a zero a la demande), puis croissance par ajouts.
    let big = vec![0u64; 2 << 20];
    ensure(big.iter().all(|&x| x == 0), "vec![0; N] non nul")?;
    drop(big);
    let mut v = Vec::new();
    for i in 0..1_000_000u32 {
        v.push(i);
    }
    ensure(v.iter().map(|&x| x as u64).sum::<u64>() == 499_999_500_000, "somme")?;
    drop(v);
    // Beaucoup de petits blocs, liberes dans le desordre.
    let mut boxes: Vec<Box<[u8]>> = (0..20_000).map(|i| vec![i as u8; 1 + i % 300].into_boxed_slice()).collect();
    for i in (0..boxes.len()).step_by(3) {
        boxes[i] = Box::new([]);
    }
    ensure(boxes.iter().enumerate().all(|(i, b)| b.is_empty() || b[0] == i as u8), "contenu")?;
    drop(boxes);
    // Alignements forts.
    for align in [16usize, 64, 4096, 65536] {
        let l = Layout::from_size_align(100, align).unwrap();
        let p = unsafe { alloc(l) };
        ensure(!p.is_null() && p as usize % align == 0, format!("alignement {align}"))?;
        unsafe {
            p.write_bytes(0xAB, 100);
            dealloc(p, l);
        }
    }
    // Grands blocs alloues et rendus en boucle : les adresses sont reemployees.
    for _ in 0..64 {
        let b = vec![1u8; 2 << 20];
        hint::black_box(&b);
    }
    let s: String = (0..1000).map(|i| format!("{i},")).collect();
    ensure(s.len() == 3890, "format")?;
    Ok("16 Mio, 1M ajouts, 20k blocs, alignements, 64 x 2 Mio".into())
}

fn collections() -> R {
    let mut h = HashMap::new();
    for i in 0..10_000u32 {
        h.insert(format!("cle{i}"), i);
    }
    ensure(h.len() == 10_000 && h["cle1234"] == 1234, "HashMap")?;
    let b: BTreeMap<u32, u32> = (0..1000).rev().map(|i| (i, i * i)).collect();
    ensure(b.keys().next() == Some(&0) && b[&999] == 998_001, "BTreeMap")?;
    let mut v: Vec<i64> = (0..50_000).map(|i| (i * 7919) % 50_021).collect();
    v.sort_unstable();
    ensure(v.windows(2).all(|w| w[0] <= w[1]), "tri")?;
    Ok("HashMap 10k, BTreeMap, tri 50k".into())
}

fn math() -> R {
    let x = 2f64.sqrt() * 2f64.sqrt();
    ensure((x - 2.0).abs() < 1e-12, "sqrt")?;
    ensure((std::f64::consts::PI.sin()).abs() < 1e-12, "sin")?;
    ensure((1f64.exp().ln() - 1.0).abs() < 1e-12, "exp/ln")?;
    ensure((0.5f32.atan2(0.5) - std::f32::consts::FRAC_PI_4).abs() < 1e-6, "atan2")?;
    Ok(format!("pi = {:.10}, e = {:.10}", std::f64::consts::PI, 1f64.exp()))
}

fn files() -> R {
    let root = "/tmp/stdtest";
    let _ = fs::remove_dir_all(root);
    fs::create_dir_all(format!("{root}/a/b")).map_err(|e| format!("create_dir_all : {e}"))?;
    let p = format!("{root}/a/b/notes.txt");
    fs::write(&p, "premiere ligne\n").map_err(|e| format!("write : {e}"))?;
    {
        let mut f = fs::OpenOptions::new().append(true).open(&p).map_err(|e| format!("append : {e}"))?;
        writeln!(f, "deuxieme ligne, accentuee : ete").map_err(|e| e.to_string())?;
    }
    let text = fs::read_to_string(&p).map_err(|e| format!("read : {e}"))?;
    ensure(text == "premiere ligne\ndeuxieme ligne, accentuee : ete\n", format!("contenu {text:?}"))?;
    let lines: Vec<String> = BufReader::new(fs::File::open(&p).unwrap()).lines().map(|l| l.unwrap()).collect();
    ensure(lines.len() == 2, "lignes")?;
    let meta = fs::metadata(&p).map_err(|e| e.to_string())?;
    ensure(meta.is_file() && meta.len() == text.len() as u64, "metadata")?;
    // Position : seek puis lecture.
    let mut f = fs::File::open(&p).unwrap();
    f.seek(SeekFrom::Start(9)).unwrap();
    let mut w = [0u8; 5];
    f.read_exact(&mut w).unwrap();
    ensure(&w == b"ligne", "seek")?;
    ensure(f.seek(SeekFrom::End(-4)).unwrap() == text.len() as u64 - 4, "seek fin")?;
    // Le service FAT refuse de supprimer un fichier ouvert (EBUSY) : on ferme.
    drop(f);
    // Gros fichier : plusieurs messages par lecture et par ecriture.
    let data: Vec<u8> = (0..200_000u32).map(|i| (i * 31 % 251) as u8).collect();
    fs::write(format!("{root}/gros.bin"), &data).map_err(|e| e.to_string())?;
    ensure(fs::read(format!("{root}/gros.bin")).map_err(|e| e.to_string())? == data, "gros fichier")?;
    // Copie d'un fichier ouvert : position partagee, comme sous Unix.
    {
        let mut a = fs::File::open(&p).unwrap();
        let mut b = a.try_clone().map_err(|e| format!("try_clone : {e}"))?;
        let mut w = [0u8; 9];
        a.read_exact(&mut w).unwrap();
        b.read_exact(&mut w[..5]).unwrap();
        ensure(&w[..5] == b"ligne", "position partagee")?;
    }
    fs::copy(&p, format!("{root}/copie.txt")).map_err(|e| format!("copy : {e}"))?;
    fs::rename(format!("{root}/copie.txt"), format!("{root}/a/renomme.txt")).map_err(|e| format!("rename : {e}"))?;
    let mut names: Vec<String> =
        fs::read_dir(format!("{root}/a")).unwrap().map(|e| e.unwrap().file_name().into_string().unwrap()).collect();
    names.sort();
    ensure(names == ["b", "renomme.txt"], format!("read_dir {names:?}"))?;
    // Chemins relatifs depuis le dossier courant.
    let old = env::current_dir().unwrap();
    env::set_current_dir(format!("{root}/a")).map_err(|e| e.to_string())?;
    ensure(fs::read_to_string("b/../renomme.txt").map_err(|e| e.to_string())? == text, "chemin relatif")?;
    env::set_current_dir(old).unwrap();
    ensure(fs::metadata(format!("{root}/absent")).map(|_| ()).unwrap_err().kind() == std::io::ErrorKind::NotFound, "NotFound")?;
    // Fichier garde ouvert par un programme qui s'arrete brutalement : le
    // service le ferme d'office et la suppression aboutit.
    let st = Command::new("stdtest").args(["holdfile", &p]).status().map_err(|e| e.to_string())?;
    ensure(st.code() == Some(134), format!("holdfile : {st}"))?;
    fs::remove_file(&p).map_err(|e| format!("fichier d'un programme disparu : {e}"))?;
    fs::remove_dir_all(root).map_err(|e| format!("remove_dir_all : {e}"))?;
    ensure(!fs::exists(root).unwrap(), "suppression")?;
    Ok("dossiers, ajout, seek, 200 ko, copie, renommage, relatifs".into())
}

fn processes() -> R {
    let st = Command::new("true").status().map_err(|e| format!("true : {e}"))?;
    ensure(st.success(), format!("true : {st}"))?;
    let mut child = Command::new("hello-std").arg("depuis-stdtest").spawn().map_err(|e| e.to_string())?;
    let pid = child.id();
    let st = child.wait().map_err(|e| e.to_string())?;
    ensure(st.code() == Some(0), format!("hello-std : {st}"))?;
    ensure(Command::new("absent-xyz").status().is_err(), "programme absent")?;
    Ok(format!("true, hello-std (pid {pid}), moi : pid {}", std::process::id()))
}

fn pipes() -> R {
    use std::process::Stdio;
    // output() : sortie capturee.
    let out = Command::new("hello-std").arg("tube").output().map_err(|e| format!("output : {e}"))?;
    let text = String::from_utf8_lossy(&out.stdout);
    ensure(out.status.success() && text.starts_with("Bonjour depuis std"), format!("output : {:?}", text))?;
    // Sortie et erreurs separees.
    let out = Command::new("stdtest").arg("errtest").output().map_err(|e| e.to_string())?;
    ensure(out.stdout == b"vers stdout\n" && out.stderr == b"vers stderr\n", format!("stderr : {:?} / {:?}", out.stdout, out.stderr))?;
    // Entree et sortie redirigees.
    let mut child = Command::new("stdtest")
        .arg("upper")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .map_err(|e| e.to_string())?;
    let mut w = child.stdin.take().unwrap();
    let writer = thread::spawn(move || {
        w.write_all(b"abc\n").unwrap();
        w.write_all(&[b'x'; 10_000]).unwrap();
    });
    let mut up = String::new();
    child.stdout.take().unwrap().read_to_string(&mut up).map_err(|e| e.to_string())?;
    writer.join().unwrap();
    ensure(child.wait().map_err(|e| e.to_string())?.success(), "upper : statut")?;
    ensure(up.len() == 10_004 && up.starts_with("ABC\nXXX"), format!("upper : {} octets", up.len()))?;
    // Chaine a | b : la sortie de l'un est l'entree de l'autre, sans relais.
    let mut a = Command::new("hello-std").stdout(Stdio::piped()).spawn().map_err(|e| e.to_string())?;
    let b = Command::new("stdtest").arg("upper").stdin(a.stdout.take().unwrap()).output().map_err(|e| e.to_string())?;
    a.wait().map_err(|e| e.to_string())?;
    ensure(String::from_utf8_lossy(&b.stdout).starts_with("BONJOUR DEPUIS STD"), "a | b")?;
    // Rien : sortie jetee.
    ensure(Command::new("hello-std").stdout(Stdio::null()).status().map_err(|e| e.to_string())?.success(), "null")?;
    // io::pipe() vers un enfant.
    let (mut r, w) = std::io::pipe().map_err(|e| e.to_string())?;
    let mut c = Command::new("hello-std").stdout(w).spawn().map_err(|e| e.to_string())?;
    let mut got = String::new();
    r.read_to_string(&mut got).map_err(|e| e.to_string())?;
    c.wait().map_err(|e| e.to_string())?;
    ensure(got.contains("arguments"), "io::pipe")?;
    Ok("output, stderr, entree+sortie 10 ko, a | b, null, io::pipe".into())
}

fn network() -> R {
    let addr = ("example.com", 80).to_socket_addrs().map_err(|e| format!("DNS : {e}"))?.next().unwrap();
    let mut s = TcpStream::connect_timeout(&addr, Duration::from_secs(10)).map_err(|e| format!("connect : {e}"))?;
    s.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    s.write_all(b"GET / HTTP/1.0\r\nHost: example.com\r\n\r\n").map_err(|e| e.to_string())?;
    let mut resp = Vec::new();
    s.read_to_end(&mut resp).map_err(|e| format!("lecture : {e}"))?;
    let head = String::from_utf8_lossy(&resp[..resp.len().min(12)]).into_owned();
    ensure(head.starts_with("HTTP/1."), format!("reponse {head:?}"))?;
    // Copie d'un flux (try_clone) : lecture et ecriture depuis deux threads.
    let s2 = TcpStream::connect_timeout(&addr, Duration::from_secs(10)).map_err(|e| format!("connect 2 : {e}"))?;
    let mut w = s2.try_clone().map_err(|e| format!("try_clone : {e}"))?;
    let reader = thread::spawn(move || {
        let mut s2 = s2;
        let mut v = Vec::new();
        s2.read_to_end(&mut v).map(|_| v.len())
    });
    w.write_all(b"HEAD / HTTP/1.0\r\nHost: example.com\r\n\r\n").map_err(|e| e.to_string())?;
    drop(w);
    let n2 = reader.join().unwrap().map_err(|e| format!("lecture clone : {e}"))?;
    ensure(n2 > 0, "clone : reponse vide")?;
    // Port garde par un programme disparu : repris.
    let st = Command::new("stdtest").args(["holdport", "8765"]).status().map_err(|e| e.to_string())?;
    ensure(st.code() == Some(134), format!("holdport : {st}"))?;
    drop(std::net::TcpListener::bind("0.0.0.0:8765").map_err(|e| format!("port d'un programme disparu : {e}"))?);
    let u = UdpSocket::bind("0.0.0.0:0").map_err(|e| e.to_string())?;
    let local = u.local_addr().unwrap();
    Ok(format!("{addr} : {} octets ({}), UDP {local}", resp.len(), head.trim()))
}

fn bench() {
    let n = 100_000;
    let t = Instant::now();
    for _ in 0..n {
        hint::black_box(Instant::now());
    }
    println!("  Instant::now          {:>8.0} ns", t.elapsed().as_nanos() as f64 / n as f64);
    let m = Mutex::new(0u64);
    let t = Instant::now();
    for _ in 0..n {
        *m.lock().unwrap() += 1;
    }
    println!("  Mutex lock+unlock     {:>8.0} ns", t.elapsed().as_nanos() as f64 / n as f64);
    let t = Instant::now();
    for i in 0..n {
        hint::black_box(Box::new(i));
    }
    println!("  Box::new + drop       {:>8.0} ns", t.elapsed().as_nanos() as f64 / n as f64);
    let k = 200;
    let t = Instant::now();
    for _ in 0..k {
        thread::spawn(|| hint::black_box(1)).join().unwrap();
    }
    println!("  thread spawn+join     {:>8.1} us", t.elapsed().as_nanos() as f64 / k as f64 / 1000.0);
    let (tx, rx) = mpsc::channel::<u32>();
    let (tx2, rx2) = mpsc::channel::<u32>();
    let h = thread::spawn(move || {
        for v in rx.iter() {
            tx2.send(v).unwrap();
        }
    });
    let t = Instant::now();
    for i in 0..10_000 {
        tx.send(i).unwrap();
        rx2.recv().unwrap();
    }
    println!("  canal aller-retour    {:>8.1} us", t.elapsed().as_nanos() as f64 / 10_000.0 / 1000.0);
    drop(tx);
    h.join().unwrap();
}

/// `stdtest serve PORT N` : petit serveur HTTP (un thread par connexion)
/// pour verifier `TcpListener` depuis l'hote ; s'arrete apres N reponses.
fn serve(port: u16, count: usize) {
    let l = std::net::TcpListener::bind(("0.0.0.0", port)).expect("bind");
    println!("stdtest : ecoute sur {}", l.local_addr().unwrap());
    let served = Arc::new(AtomicUsize::new(0));
    let mut hs = Vec::new();
    for _ in 0..count {
        let (mut s, peer) = l.accept().expect("accept");
        let served = served.clone();
        hs.push(thread::spawn(move || {
            let mut req = [0u8; 1024];
            let n = s.read(&mut req).unwrap_or(0);
            let first = String::from_utf8_lossy(&req[..n]).lines().next().unwrap_or("").to_string();
            let k = served.fetch_add(1, Ordering::SeqCst) + 1;
            let body = format!("bonjour de std sur nxk, reponse {k} a {peer} ({first})\n");
            let resp = format!("HTTP/1.0 200 OK\r\nContent-Length: {}\r\n\r\n{body}", body.len());
            s.write_all(resp.as_bytes()).unwrap();
        }));
    }
    for h in hs {
        h.join().unwrap();
    }
    println!("RESULT stdtest_serve {} reponses", served.load(Ordering::SeqCst));
}

fn main() {
    let args: Vec<String> = env::args().collect();
    match args.get(1).map(String::as_str) {
        // Outils des tests de tubes.
        Some("upper") => {
            let mut v = Vec::new();
            std::io::stdin().read_to_end(&mut v).unwrap();
            std::io::stdout().write_all(&v.to_ascii_uppercase()).unwrap();
            return;
        }
        // Garde une ressource puis s'arrete brutalement (abort) : le service
        // doit la liberer d'office.
        Some("holdfile") => {
            let _f = fs::File::open(&args[2]).unwrap();
            std::process::abort();
        }
        Some("holdport") => {
            let _l = std::net::TcpListener::bind(("0.0.0.0", args[2].parse::<u16>().unwrap())).unwrap();
            std::process::abort();
        }
        Some("errtest") => {
            use std::io::IsTerminal;
            // Redirigees, les sorties ne sont pas des terminaux.
            let tty = std::io::stdout().is_terminal() || std::io::stderr().is_terminal();
            println!("{}", if tty { "terminal ?" } else { "vers stdout" });
            eprintln!("vers stderr");
            return;
        }
        _ => {}
    }
    if args.get(1).map(String::as_str) == Some("serve") {
        let port = args.get(2).and_then(|p| p.parse().ok()).unwrap_or(8080);
        let count = args.get(3).and_then(|p| p.parse().ok()).unwrap_or(3);
        serve(port, count);
        return;
    }
    let with_net = args.iter().any(|a| a == "net");
    if args.iter().any(|a| a == "bench") {
        println!("stdtest : mesures");
        bench();
        return;
    }
    let mut tests: Vec<(&str, fn() -> R)> = vec![
        ("threads", threads),
        ("scope", scope),
        ("mutex", mutex),
        ("condvar", condvar),
        ("rwlock", rwlock),
        ("canaux", channels),
        ("barriere", barrier_once),
        ("thread_local", thread_locals),
        ("park", park),
        ("temps", sleep_time),
        ("environnement", environment),
        ("memoire", memory),
        ("collections", collections),
        ("maths", math),
        ("fichiers", files),
        ("processus", processes),
        ("tubes", pipes),
    ];
    if with_net {
        tests.push(("reseau", network));
    }
    println!("stdtest : {} tests", tests.len());
    let t0 = Instant::now();
    let mut ok = 0;
    for (name, f) in &tests {
        let t = Instant::now();
        match f() {
            Ok(detail) => {
                ok += 1;
                println!("  ok     {name:<14} {detail} [{} ms]", t.elapsed().as_millis());
            }
            Err(e) => println!("  ECHEC  {name:<14} {e}"),
        }
    }
    println!("RESULT stdtest {ok}/{} reussis en {} ms", tests.len(), t0.elapsed().as_millis());
    std::process::exit(if ok == tests.len() { 0 } else { 1 });
}
