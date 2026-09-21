# nxk — micro-noyau AArch64 pour Tegra X1

Micro-noyau écrit **entièrement en Rust**, sans système hôte, pour le SoC
NVIDIA Tegra X1 de la Nintendo Switch (Erista et Mariko). Il tourne directement
sur les quatre Cortex-A57, orienté serveur : pas d'interface graphique, un shell
sur la console série.

Comme Linux, il est l'interface entre le matériel et les programmes, qui
s'écrivent au-dessus d'appels système. Mais en vrai micro-noyau, il ne garde que
ce qui exige le privilège EL1 : espaces d'adressage, threads, ordonnancement,
IPC, capacités, routage des interruptions. **Les pilotes et les services
(fichiers, réseau…) sont des programmes ordinaires en EL0**, avec pour seuls
droits les capacités qu'on leur a confiées.

Les programmes s'écrivent en **Rust ordinaire, avec la bibliothèque standard**
(`std` est portée : cible `aarch64-unknown-nxk`). Les logiciels applicatifs
(serveur web, SSH…) n'appartiennent pas au système : ce sont des programmes
comme les autres.

```
EL0  programmes Rust (std) · shell
     services : fs (FAT32) · net (TCP/IP) · display · input · power
     pilotes  : console (UART) · sd · usb (xHCI) · i2c · clk
     init : gestionnaire de processus et service de noms
     ─────────── std (portée) · libnx (no_std) ───────────
     ─────────── svc #0 : 51 appels système ───────────
EL1  nxk : mémoire · threads · ordonnanceur · IPC · capacités · IRQ · futex
```

Tout a été vérifié dans l'émulateur `tegra_qemu`, **jamais sur une vraie
Switch** (voir « Limites »).

## Compiler, lancer, tester

Prérequis : Rust stable avec `rust-src`
(`rustup component add rust-src` ; cibles `aarch64-unknown-none` et
`aarch64-unknown-none-softfloat`), l'émulateur `tegra_qemu` compilé, `mkfs.fat`
et `pyfatfs` pour l'image de carte SD.

```bash
./tools/build.sh                  # chaîne std, programmes, image de démarrage, noyau
./tools/mksd.py -o build/sd.img   # carte SD FAT32 (programmes dans /bin)
./tools/run.sh                    # démarre ; shell sur le terminal (Ctrl-A X pour quitter)
./tools/nxtest.py -c ktest -c stdtest --sd build/sd.img   # tests automatisés
```

Options de `nxtest.py` : `--sd IMAGE` (carte SD), `--net [REDIRECTIONS]`
(carte réseau USB, ex. `tcp::8080-:80`), `--usb-kbd` et `--keys` (clavier
USB), `--machine tegrax1plus` (Mariko), `--monitor ELF --dtb DTB` (démarrage en
EL2 sous un moniteur PSCI), `--screendump` (capture de l'écran).

Les dossiers de compilation vivent dans `~/.cache/nxk/` : le projet est
synchronisé et n'a pas à transporter d'artefacts.

## Écrire un programme

Un programme est une crate Rust ordinaire dans `apps/`, ajoutée à
`apps/Cargo.toml` et à la liste `apps` de `tools/build.sh` :

```rust
use std::io::{Read, Write};
use std::net::TcpListener;
use std::thread;

fn main() -> std::io::Result<()> {
    let l = TcpListener::bind("0.0.0.0:8080")?;
    for s in l.incoming() {
        let mut s = s?;
        thread::spawn(move || {
            let mut req = [0u8; 1024];
            let _ = s.read(&mut req);
            let _ = s.write_all(b"HTTP/1.0 200 OK\r\n\r\nbonjour de nxk\n");
        });
    }
    Ok(())
}
```

`tools/mkstd.py` prépare la chaîne `nxk` : une copie de Rust stable (liens
durs) dont la bibliothèque standard reçoit le portage de `std/library`, la
cible `std/aarch64-unknown-nxk.json` et sa déclaration à rustup. `build.sh`
compile ensuite `apps/` ; à la main :

```bash
cd apps && RUSTC_BOOTSTRAP=1 CARGO_TARGET_DIR=~/.cache/nxk/target-apps cargo build --release
```

(`RUSTC_BOOTSTRAP` pour `-Z build-std`, voir `apps/.cargo/config.toml` ; ce
dossier de compilation est celui dont `mkstd.py` invalide la `std` quand le
portage change.) Les programmes démarrent depuis l'image de démarrage (par
leur nom) ou depuis la carte SD (`/bin/NOM`, ou un chemin).

### Ce que fournit `std`

| Domaine | Pris en charge | Absent |
|---|---|---|
| Mémoire | `Box`, `Vec`, collections ; allocateur à 36 classes de taille (O(1), sans en-tête), grands blocs rendus au noyau, adresses réemployées | |
| Threads | `spawn`, `scope`, noms, `join`, `park`, `sleep`, `available_parallelism`, `thread_local!` avec destructeurs ; piles gardées pour les threads suivants ; `std::os::nxk::thread` (priorité, cœur) | |
| Synchronisation | `Mutex`, `RwLock`, `Condvar`, `Once`, `OnceLock`, `Barrier`, `mpsc` (futex du noyau) | |
| Temps | `Instant` sans appel système, `SystemTime` (horloge temps réel) | |
| Environnement | `args`, variables, dossier courant (`PWD`), `consts::OS = "nxk"`, `getrandom` pour `HashMap` | `current_exe` approché |
| Fichiers | FAT32 : lecture, écriture, ajout, `seek`, dossiers, `rename`, `copy`, `remove_dir_all`, `metadata` (taille, date), `try_clone` | liens, droits, `set_times`, verrous |
| Réseau | IPv4 : `TcpStream`, `TcpListener`, `UdpSocket`, délais, mode non bloquant, `try_clone`, résolution DNS | IPv6, `peek`, multicast, boucle locale |
| Processus | `Command` : arguments, environnement, dossier ; `spawn`, `status`, `output`, `wait`, `try_wait`, `kill`, `id` ; tubes (`piped`, `io::pipe`, `a \| b` sans relais), `null` | redirection vers un fichier |
| E/S standard | console ou tubes, `IsTerminal` | |
| Paniques | message sur la sortie d'erreurs, arrêt (`panic=abort`) | déroulement, traces d'appels |

## Organisation

```
abi/        nxk-abi : numéros d'appels, erreurs, droits, protocoles des services
            (partagé par le noyau, libnx et std : une seule source de vérité)
kernel/     le micro-noyau
user/       libnx (bibliothèque système no_std), init, shell, pilotes et services
std/        portage de la bibliothèque standard Rust, script de liaison, cible
apps/       programmes Rust ordinaires (std) : hello-std, stdtest
bench/      comparaison avec L4T : moniteur EL3, programme de mesure Linux, scripts
tools/      construction, carte SD, chaîne std, lancement, tests automatisés
refs/       hekate, Atmosphère : documentation du matériel (jamais compilés)
```

Environ 8 000 lignes de Rust pour le noyau, 7 000 pour les pilotes, services et
libnx, 3 800 pour le portage de `std`.

## Appels système

Conventions de Linux arm64 : `svc #0`, numéro dans `x8`, arguments dans
`x0`–`x5`, résultat dans `x0`, erreurs en `-errno` (valeurs Linux). Les appels
bloquants prennent un délai (ns ; 0 = sans limite).

| Famille | Appels |
|---|---|
| Base | `nop`, `yield`, `sleep`, `clock` (monotone, temps réel), `clock_set`, `getcpu`, `getpid`, `gettid`, `getrandom`, `debug_write`, `thread_exit`, `process_exit`, `system_power` |
| Handles | `handle_close`, `handle_dup` (droits réduits, insigne), `handle_revoke` |
| Mémoire | `mem_map` (à la demande ou peuplée, adresse libre ou imposée, W^X), `mem_unmap`, `mem_protect`, `shm_create`/`shm_map`/`shm_paddr` (contiguë, non cachée, sous 4 GiB pour le DMA), `mmio_map`, `mmio_derive` |
| Threads | `thread_create`, `thread_set` (priorité, cœur), `futex_wait`, `futex_wake` |
| Processus | `process_spawn` (ELF static-PIE, arguments, environnement), `process_wait`, `process_kill` |
| IPC | `endpoint_create` (option tube), `ipc_call`, `ipc_send`, `ipc_recv`, `ipc_reply`, `ipc_reply_recv`, `ipc_reply_to` |
| Notifications, IRQ | `notify_create`, `notify_signal`, `notify_wait`, `notify_bind`, `irq_get`, `irq_bind`, `irq_ack` |
| Observation, débogage | `sysinfo` (processus, mémoire, cœurs), `debug_attach`, `debug_regs`, `debug_mem`, `debug_threads`, `debug_stop` |

L'heure se lit sans appel système : le noyau ouvre `CNTVCT_EL0` à EL0 (comme le
vDSO de Linux).

## Conception du noyau

**Une pile noyau par cœur, pas par thread** (modèle de seL4). Les registres
sont rangés directement dans le bloc de contrôle du thread ; le noyau ne bloque
jamais au milieu d'un appel.

**Un verrou global**, comme seL4 en multiprocesseur : les chemins noyau durent
quelques microsecondes. Les appels sans état partagé (`clock`, `getcpu`,
`gettid`) passent par un chemin rapide sans verrou.

**Capacités** : tout objet (endpoint, thread, processus, notification,
mémoire partagée, ligne d'interruption, plage de registres) se désigne par un
handle portant des droits et une génération ; révocation par changement de
génération. Tables d'objets extensibles.

**IPC synchrone** : 6 mots dans les registres, jusqu'à 4 KiB par la page de
contrôle (UTCB) de chaque thread, jusqu'à 4 handles transmis ; le noyau fournit
à chaque message l'insigne et le pid de l'émetteur. Client et serveur sur le
même cœur : bascule directe sans l'ordonnanceur. Délais sur tous les appels
bloquants. Un endpoint « tube » signale la fin du flux quand son dernier
écrivain disparaît.

**Ordonnanceur sans tic** : 32 priorités, sélection O(1), une file par cœur ;
le temporisateur est programmé sur la prochaine échéance (réveil, ou fin de
quantum seulement si un thread de même priorité attend). Équilibrage vers les
cœurs au repos, migration, affinité et priorité modifiables. Un cœur sans
travail ne reçoit pas d'interruption périodique (les services non plus ne
s'éveillent pas d'eux-mêmes).

**Mémoire** : buddy (4 KiB–4 MiB) ; pages mises à zéro par `DC ZVA` ; espaces
d'adressage étiquetés par ASID ; pages anonymes et piles allouées au premier
accès (y compris par plusieurs cœurs à la fois) ; W^X imposé ; ASLR
(exécutables static-PIE relogés par le chargeur, tas, piles, registres) ; zones
réservées (carveouts du contrôleur mémoire, `/reserved-memory` de l'arbre de
périphériques) exclues.

**Sécurité** : mitigation Spectre v2 (prédicteur invalidé au changement de
processus), W^X, ASLR, isolation des fautes (un programme fautif est terminé,
le reste continue).

**Débogage** : un débogueur (endpoint) reçoit fautes, points d'arrêt et pas à
pas d'un processus, lit et écrit registres et mémoire, arrête des threads.

**Démarrage** : directement depuis le reset (EL3), ou en EL2 sous un moniteur
PSCI (comme sur une Switch avec son firmware sécurisé) ; taille de la DRAM
détectée ; arbre de périphériques lu s'il est fourni.

## Services et pilotes

Tous en EL0, lancés par `init` avec leurs seules capacités.

| Service | Rôle | État |
|---|---|---|
| `console` | UART-A, édition de ligne | émulateur |
| `clk` | horloges et resets (CAR) | émulateur |
| `i2c` | six bus I2C | émulateur |
| `power` | extinction et redémarrage (PMIC MAX77620), horloge temps réel, températures (TMP451) | émulateur |
| `sd` | carte SD (SDHCI, ADMA2) | émulateur ; alimentation et broches du vrai lecteur non réglées |
| `fs` | FAT32 (noms longs), cache en écriture différée avec lecture anticipée | émulateur |
| `display` | écran (fenêtre A), terminal texte (police Terminus) | émulateur ; initialisation du panneau réel absente |
| `input` | écran tactile (FTM3BD56), clavier USB | émulateur ; interrogé seulement quand quelqu'un lit |
| `usb` | xHCI : clavier HID, carte réseau CDC-ECM | émulateur ; le vrai xHCI exige un firmware NVIDIA |
| `net` | TCP/IP (smoltcp) : DHCP, DNS, TCP, UDP | émulateur |

Les services `fs` et `net` rattachent chaque fichier ou socket au processus
qui l'a ouvert : un autre ne peut pas s'en servir, et ceux d'un programme
disparu sans les fermer (plantage) sont repris.

## Fonctionnalités

| Domaine | Fait | Reste |
|---|---|---|
| Démarrage | 4 cœurs, EL3 ou EL2 + PSCI, Erista et Mariko, détection de la DRAM | démarrage sur une vraie Switch |
| Mémoire | à la demande, W^X, `mprotect`, mémoire partagée, tampons DMA, ASLR, zones réservées | |
| Threads | priorités, affinité, migration, équilibrage, sans tic, futex | états de veille profonde |
| IPC | synchrone, délais, handles, notifications, tubes, identité de l'émetteur | attente sur plusieurs objets (`poll`) |
| Programmes | ELF static-PIE, arguments, environnement, TLS, chargement depuis la carte SD | liaison dynamique (écartée) |
| Rust | `no_std` (libnx) et **`std`** (voir « Ce que fournit `std` ») | déroulement des paniques, traces d'appels |
| Stockage | SD, FAT32, API fichiers | eMMC, ext4 |
| Réseau | USB-Ethernet, TCP/IP, sockets | Wi-Fi, IPv6 |
| Matériel | UART, I2C, PMIC, RTC, températures, écran, tactile, USB | GPU, audio, Joy-Con, boutons (GPIO), fréquences (DVFS), batterie |
| Sécurité | capacités, révocation, Spectre v2, isolation | |
| Outils | construction, carte SD, tests (`ktest` 27, `stdtest` 18), débogueur (gdb), comparaison avec L4T | |

## Limites

- **Rien n'a tourné sur une vraie Switch.** Le vrai contrôleur USB (xHCI)
  exige un firmware propriétaire de NVIDIA ; le lecteur SD, le réglage de
  l'alimentation et des broches ; l'écran, l'initialisation du panneau DSI ;
  les boutons, des GPIO que l'émulateur ne sait pas actionner.
- GPU, audio, Wi-Fi, Joy-Con et changement de fréquence ne sont pas faits
  (risque d'endommager le matériel pour le dernier).
- Un tube n'a pas de tampon : l'écrivain attend qu'un lecteur prenne ses
  données. `output()` lit les deux sorties en parallèle pour cette raison.
- Le service `fs` refuse de supprimer un fichier encore ouvert (`EBUSY`), comme
  Windows et non comme Unix.

## Pièges propres au matériel

1. Le GIC démarre avec toutes ses lignes en Groupe 0 (monde sécurisé) : un
   noyau en EL1 non sécurisé ne reçoit rien tant qu'on ne les bascule pas en
   Groupe 1 depuis EL3 — rôle du moniteur sécurisé sur un vrai système.
2. Réveiller un cœur secondaire exige, dans l'ordre : vecteur de reset dans le
   bloc SB, partition d'alimentation au PMC, retrait des verrous de tension,
   lignes de reset du CAR.

## Comparaison avec L4T

Voir `bench/README.md` et `bench/RESULTS.md`. Dans l'émulateur, créer puis
attendre un thread coûte environ 300 µs avec `std` sur nxk (226 µs avec libnx),
contre 836 µs avec pthread sous L4T.

## Licences

La police de l'écran vient de Terminus (Dimitar Zhekov, SIL Open Font License
1.1), convertie à la construction depuis la police console du système hôte.
