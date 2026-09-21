# Comparaison nxk / L4T

Les deux systèmes tournent sur **la même machine émulée** (`tegra_qemu`,
machine `tegrax1` : 4 Cortex-A57, 4 GiB) et exécutent **les mêmes mesures** :
même code utilisateur, même horloge (`CNTVCT_EL0`), mêmes itérations.

## Ce qui est comparé

| Côté | Noyau | Programme de mesure |
|---|---|---|
| nxk | ce dépôt | `user/bench` (IPC nxk : `call` / `reply_recv`) |
| L4T | Linux 4.9.140-l4t de Switchroot 5.1.2 (mai 2026), inchangé | `bench/lbench`, `/init` statique sans libc (tubes et futex Linux) |

Mesures : appel système nul ; aller-retour requête/réponse entre deux threads
(même coeur, deux coeurs) ; débit par messages de 4 000 octets ; création +
attente d'un thread ; lancement + attente d'un programme (chargement ELF
compris ; `vfork`+`execve`+`wait4` côté Linux) ; un calcul entier pur qui ne
dépend pas du noyau et sert de témoin ; mémoire occupée après démarrage.

## Comment L4T démarre dans l'émulateur

Sur une vraie console, la chaîne est hekate → ATF (bl31) → U-Boot (bl33) →
noyau. Dans l'émulateur, `bench/l4tboot` la remplace : un moniteur EL3 de
quelques centaines de lignes qui bascule le GIC en mode non sécurisé, fournit
PSCI 1.0 (dont `CPU_ON`, avec la séquence de réveil des coeurs validée dans
nxk) et saute dans le noyau avec l'arbre de périphériques. Linux démarre ses
quatre coeurs par PSCI comme sur le matériel.

L'arbre de périphériques est celui de Switchroot (`nx-plat.dtimg`, modèle
2017), corrigé comme le fait `boot.scr` : 4 GiB de RAM, UART-A en console,
initramfs. Pour démarrer sans écran dans l'émulateur :

- `l4t-headless.disable` désactive les périphériques que l'émulateur ne modélise
  pas assez pour Linux (PMIC et régulateurs I2C, DVFS, écran, GPU, USB, audio,
  carte SD, chien de garde…). Le noyau mesuré est inchangé : seuls des pilotes
  de périphériques sont écartés.
- `cpuidle.off=1 idle=poll` : sous l'émulateur, les coeurs Linux endormis par
  `wfi` via le contrôleur de flux Tegra ne se réveillent pas. En `idle=poll`,
  un coeur inactif scrute en boucle au lieu de dormir. Pour comparer à
  configuration égale, nxk est aussi mesuré en `idle-poll`
  (`NXK_FEATURES=idle-poll ./tools/build.sh`).

## Reproduire

```bash
# nxk (3 executions)
NXK_FEATURES=idle-poll ./tools/build.sh
for i in 1 2 3; do ./tools/nxtest.py -c bench > nxk-$i.log; done

# L4T : Image, nx-plat.dtimg extraits de l'archive Switchroot
python3 bench/mkinitramfs.py --init <lbench> -o initramfs.cpio
python3 bench/mkdtb.py --dtimg nx-plat.dtimg --initrd-start 0x92000000 \
    --initrd-size $(stat -c%s initramfs.cpio) \
    --disable "$(grep -v '^#' bench/l4t-headless.disable | paste -sd,)" \
    --bootargs "console=ttyS0,115200n8 rdinit=/init cpuidle.off=1 idle=poll" -o board.dtb
python3 bench/run_l4t.py --dir <dossier> --until "RESULT END" --log l4t-1.log

python3 bench/compare.py --nxk nxk-*.log --l4t l4t-*.log --md bench/RESULTS.md
```

## Limites

- **Émulation.** QEMU traduit dynamiquement les instructions : une entrée
  dans le noyau, une instruction atomique ou un réveil de coeur y coûtent bien
  plus cher que sur le silicium, et les caches ne sont pas modélisés. Les
  valeurs absolues ne valent que pour l'émulateur ; les rapports entre les
  deux systèmes, mesurés dans les mêmes conditions, sont l'information utile.
- **L4T minimal.** Le noyau L4T est mesuré avec un seul programme en espace
  utilisateur. Une installation réelle démarre Ubuntu avec son environnement
  graphique, qui ajoute plusieurs centaines de Mo : les chiffres mémoire de
  L4T sont donc un plancher.
- **Mécanismes différents.** L'IPC de nxk (`call`/`reply_recv`) est comparée
  aux deux mécanismes Linux les plus proches : les tubes (le plus courant) et
  les futex (le plus rapide, sans transport de données).
