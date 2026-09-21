# Résultats : nxk contre L4T

Mesures du 21 septembre 2026. Même machine émulée (`tegra_qemu`, Tegra X1,
4 Cortex-A57, 4 GiB), mêmes programmes de mesure, médiane de 3 exécutions par
système. Méthode et moyens de reproduction : `bench/README.md`.

- **nxk** : ce dépôt, configuration par défaut (coeurs au repos en `wfi`).
- **L4T** : Linux 4.9.140-l4t de Switchroot 5.1.2 (4 mai 2026), noyau inchangé,
  arbre de périphériques officiel de la Switch 2017 ; `cpuidle.off=1` et les
  périphériques non émulés désactivés (`l4t-headless.disable`).

| Mesure | nxk | L4T | Rapport |
|---|---:|---:|---:|
| Demarrage jusqu'au premier programme (ms) | 596 | 5 560 | nxk x9.3 |
| Appel systeme nul (ns) | 1 646 | 2 085 | nxk x1.3 |
| IPC aller-retour meme coeur (ns) - L4T : tube | 4 927 | 41 828 | nxk x8.5 |
| IPC aller-retour meme coeur (ns) - L4T : futex | 4 927 | 29 728 | nxk x6.0 |
| IPC aller-retour 2 coeurs (ns) - L4T : tube | 67 197 | 102 999 | nxk x1.5 |
| IPC aller-retour 2 coeurs (ns) - L4T : futex | 67 197 | 113 271 | nxk x1.7 |
| Debit, messages de 4 KiB (Mo/s) | 108 | 89 | nxk x1.2 |
| Creation + attente d'un thread (us) | 226 | 836 | nxk x3.7 |
| Lancement + attente d'un programme (us) | 787 | 1 765 | nxk x2.2 |
| Calcul pur, temoin (ms) | 66 | 60 | L4T x1.1 |
| RAM occupee par le systeme, noyau compris (KiB) | 1 096 | 122 180 | nxk x111.5 |
| Noyau en memoire (KiB) | 740 | 23 035 | nxk x31.1 |
| RAM du programme de mesure (KiB) | 64 | 52 | L4T x1.2 |

Rapport = combien de fois le premier est meilleur que l'autre.

## Lecture

**IPC, là où un micro-noyau se joue.** Sur un même coeur, un aller-retour
nxk coûte 6 fois moins qu'un futex Linux et 8,5 fois moins qu'un tube : deux
appels système (`call`, `reply_recv`) et une commutation directe de thread à
thread, contre quatre appels et un passage par l'ordonnanceur CFS côté Linux.
Entre deux coeurs, l'écart se réduit (1,5 à 1,7) car les deux paient le réveil
d'un coeur endormi par IPI, que l'émulateur rend très coûteux.

**Appel système nul : 1,3.** Le coût est surtout celui de l'entrée et de la
sortie d'exception, identique pour les deux ; l'écart vient du chemin noyau
lui-même (chemin rapide sans verrou côté nxk).

**Mémoire : deux ordres de grandeur.** Après démarrage, tout le système nxk
(noyau, métadonnées, init, pilote console, shell) occupe 1,1 MiB. Le noyau L4T
seul, avec un unique programme en espace utilisateur, en occupe 119 MiB :
23 MiB d'image résidente, ~64 MiB pour le tableau `struct page` des 4 GiB, les
zones réservées, 12,5 MiB de slab. S'y ajoutent **512 MiB de CMA** réservés aux
périphériques (utilisables seulement par des pages déplaçables).

**Témoin.** Le calcul pur, qui ne dépend pas du noyau, donne le même temps
des deux côtés (écart dans le bruit de l'émulateur) : les deux systèmes ont
bien été mesurés dans les mêmes conditions.

## Limites

- **Émulation.** Les valeurs absolues ne valent que pour l'émulateur (entrées
  noyau, atomiques et réveils de coeurs y coûtent bien plus cher que sur le
  silicium ; pas de caches). Les rapports sont l'information utile.
- **L4T minimal.** Une vraie installation L4T démarre Ubuntu et son bureau :
  plusieurs centaines de MiB de plus. Les chiffres mémoire de L4T sont un
  plancher, pas une valeur typique.
- **Périmètre fonctionnel.** L4T embarque des milliers de pilotes, une pile
  réseau, des systèmes de fichiers, un GPU. nxk n'a aujourd'hui qu'un pilote
  (la console série) : la comparaison porte sur les mécanismes du noyau, pas
  sur l'étendue des services rendus.
