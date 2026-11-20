import { createElement, Fragment, type ReactNode } from "react";

/**
 * The app's texts, in each language it speaks. English is the reference:
 * every other language must have the same keys with the same parameters,
 * which TypeScript checks. A message that takes values is a function, so
 * each language puts them where its grammar wants. Messages that return
 * `ReactNode` take parts that may be markup (a name in bold).
 *
 * Errors coming from the daemon stay in English: they are text it sends.
 */

/** The languages, in the order they are tried after the system's. */
export type Lang = "en" | "fr";

/** `count` followed by the word that agrees with it. */
const counted = (lang: Lang, n: number, one: string, other: string) =>
  `${n} ${new Intl.PluralRules(lang).select(n) === "one" ? one : other}`;

/** Text and markup, without the keys React asks of arrays. */
const parts = (...xs: ReactNode[]) => createElement(Fragment, null, ...xs);

const en = {
  starting: "Starting crossdrop…",
  unreachable: (error: string) => `crossdrop is not reachable: ${error}`,
  sendTo: (device: string) => `Send to ${device}`,
  shareInput: "Share mouse and keyboard",
  shareOn: (key: string) => `Push the cursor past an edge to switch. ${key} comes back.`,
  off: "Off",
  layoutError: (error: string) =>
    `The desk layout could not be read, so no neighbour is shown: ${error}. Fix or delete the file, then place your devices again.`,
  dismiss: "Dismiss",
  desk: "Your desk",
  side: { top: "Above", left: "Left", right: "Right", bottom: "Below" },
  /** Where a neighbour sits, inside a sentence. */
  sideIn: { top: "above", left: "on the left", right: "on the right", bottom: "below" },
  dropHere: "Drop files here",
  offline: "Offline",
  online: "Online",
  removeFrom: (name: string, side: string) => `Remove ${name}, ${side}`,
  remove: "Remove",
  choose: "Choose…",
  pairFirst: "Pair one first",
  thisComputer: "This computer",
  deskHint:
    "Put each computer where it sits on your desk. With sharing on, the cursor crosses that edge; drag a file to the edge of the screen to drop it on that computer's desktop.",
  devices: "Devices",
  noPaired: "No paired device yet. Computers running crossdrop on this network appear below.",
  nearby: "Nearby",
  pair: "Pair",
  waitingForYou: "Waiting for you",
  nothingWaiting: "Nothing waiting.",
  wantsToPair: (name: ReactNode) => parts(name, " wants to pair. Check that it shows this code:"),
  codesMatchPair: "Codes match, pair",
  reject: "Reject",
  files: (n: number) => counted("en", n, "file", "files"),
  wantsToSend: (name: ReactNode, files: string, size: string) => parts(name, ` wants to send ${files}, ${size}`),
  receive: "Receive",
  decline: "Decline",
  transfers: "Transfers",
  clearFinished: "Clear finished",
  dropToSend: "Drop files on a device to send them.",
  to: (name: ReactNode) => parts("To ", name),
  from: (name: ReactNode) => parts("From ", name),
  toDesktop: " (desktop)",
  progress: (done: string, total: string) => `${done} of ${total}`,
  receiving: "Receiving",
  stopAndDelete: "Stop and delete what arrived?",
  stop: "Stop",
  keepReceiving: "Keep receiving",
  cancel: "Cancel",
  notifyPair: (name: string) => `${name} wants to pair`,
  notifyPairBody: (code: string) => `Accept only if it shows the code ${code}.`,
  notifyOffer: (name: string) => `${name} wants to send you files`,
  state: {
    waiting: "Waiting for them to accept",
    sending: "Sending",
    done: "Sent",
    declined: "Declined",
    failed: "Failed",
  },
  sendFiles: "Send files…",
  close: "Close",
  settings: "Settings",
  maySendFiles: "May send me files",
  autoAccept: "Receive its files without asking",
  clipboardIn: "Its copies reach my clipboard",
  clipboardOut: "My copies reach its clipboard",
  mayControl: "May control this computer's mouse and keyboard",
  lastSeen: (addr: string) => `, last seen at ${addr}`,
  forgetConfirm: (name: string) => `Forget ${name}? You will need to pair again.`,
  forget: (name: string) => `Forget ${name}`,
  startAtLogin: "Open crossdrop when I log in, so that other devices can reach this one",
  pairByAddress: "Not listed? Pair by address",
  otherComputer: "the other computer",
  pairWith: (name: string) => `Pair with ${name}`,
  compare: "Both computers show a code. Pair only if they are the same.",
  codesMatch: "Codes match",
  acceptOn: (name: string) => `Now accept on ${name}. Waiting…`,
  hide: "Hide",
  hideHint: (name: string) => `Hiding keeps the request open: it completes if ${name} accepts within two minutes.`,
  paired: (name: string) => `Paired with ${name}. You can send it files right away.`,
  done: "Done",
  refused: (name: string) => `${name} did not accept. Nothing was shared.`,
  pairingStopped: (message: string) => `Pairing stopped: ${message}`,
  /** Around the name of the computer a file is dropped on. */
  dropOnDesktop: ["Drop on ", "'s desktop"] as [string, string],
  nextComputer: "the next computer",
  /** Binary units: 1 KiB = 1024 B. */
  units: ["B", "KiB", "MiB", "GiB", "TiB"],
};

export type Messages = typeof en;

const fr: Messages = {
  starting: "Démarrage de crossdrop…",
  unreachable: (error) => `crossdrop est injoignable : ${error}`,
  sendTo: (device) => `Envoyer à ${device}`,
  shareInput: "Partager la souris et le clavier",
  shareOn: (key) => `Poussez le curseur au-delà d’un bord pour changer d’ordinateur. ${key} le ramène.`,
  off: "Désactivé",
  layoutError: (error) =>
    `La disposition du bureau n’a pas pu être lue, aucun voisin n’est donc affiché : ${error}. Corrigez ou supprimez le fichier, puis replacez vos appareils.`,
  dismiss: "Fermer",
  desk: "Votre bureau",
  side: { top: "Au-dessus", left: "À gauche", right: "À droite", bottom: "En dessous" },
  sideIn: { top: "au-dessus", left: "à gauche", right: "à droite", bottom: "en dessous" },
  dropHere: "Déposez des fichiers ici",
  offline: "Hors ligne",
  online: "En ligne",
  removeFrom: (name, side) => `Retirer ${name}, ${side}`,
  remove: "Retirer",
  choose: "Choisir…",
  pairFirst: "Associez d’abord un appareil",
  thisComputer: "Cet ordinateur",
  deskHint:
    "Placez chaque ordinateur là où il se trouve sur votre bureau. Avec le partage activé, le curseur passe par ce bord ; faites glisser un fichier au bord de l’écran pour le déposer sur le bureau de cet ordinateur.",
  devices: "Appareils",
  noPaired: "Aucun appareil associé pour l’instant. Les ordinateurs de ce réseau où crossdrop tourne apparaissent ci-dessous.",
  nearby: "À proximité",
  pair: "Associer",
  waitingForYou: "En attente de votre réponse",
  nothingWaiting: "Rien en attente.",
  wantsToPair: (name) => parts(name, " veut s’associer. Vérifiez qu’il affiche ce code :"),
  codesMatchPair: "Les codes sont identiques, associer",
  reject: "Refuser",
  files: (n) => counted("fr", n, "fichier", "fichiers"),
  wantsToSend: (name, files, size) => parts(name, ` veut envoyer ${files}, ${size}`),
  receive: "Recevoir",
  decline: "Refuser",
  transfers: "Transferts",
  clearFinished: "Effacer les terminés",
  dropToSend: "Déposez des fichiers sur un appareil pour les lui envoyer.",
  to: (name) => parts("Vers ", name),
  from: (name) => parts("De ", name),
  toDesktop: " (bureau)",
  progress: (done, total) => `${done} sur ${total}`,
  receiving: "Réception en cours",
  stopAndDelete: "Arrêter et supprimer ce qui est arrivé ?",
  stop: "Arrêter",
  keepReceiving: "Continuer la réception",
  cancel: "Annuler",
  notifyPair: (name) => `${name} veut s’associer`,
  notifyPairBody: (code) => `N’acceptez que s’il affiche le code ${code}.`,
  notifyOffer: (name) => `${name} veut vous envoyer des fichiers`,
  state: {
    waiting: "En attente de son accord",
    sending: "Envoi en cours",
    done: "Envoyé",
    declined: "Refusé",
    failed: "Échec",
  },
  sendFiles: "Envoyer des fichiers…",
  close: "Fermer",
  settings: "Réglages",
  maySendFiles: "Peut m’envoyer des fichiers",
  autoAccept: "Recevoir ses fichiers sans demander",
  clipboardIn: "Ses copies arrivent dans mon presse-papiers",
  clipboardOut: "Mes copies arrivent dans son presse-papiers",
  mayControl: "Peut contrôler la souris et le clavier de cet ordinateur",
  lastSeen: (addr) => `, vu pour la dernière fois à ${addr}`,
  forgetConfirm: (name) => `Oublier ${name} ? Il faudra l’associer à nouveau.`,
  forget: (name) => `Oublier ${name}`,
  startAtLogin: "Ouvrir crossdrop à l’ouverture de session, pour que les autres appareils puissent joindre celui-ci",
  pairByAddress: "Absent de la liste ? Associer par adresse",
  otherComputer: "l’autre ordinateur",
  pairWith: (name) => `Associer avec ${name}`,
  compare: "Les deux ordinateurs affichent un code. N’associez que s’ils sont identiques.",
  codesMatch: "Les codes sont identiques",
  acceptOn: (name) => `Acceptez maintenant sur ${name}. En attente…`,
  hide: "Masquer",
  hideHint: (name) => `La demande reste ouverte une fois masquée : elle aboutit si ${name} accepte dans les deux minutes.`,
  paired: (name) => `Associé avec ${name}. Vous pouvez lui envoyer des fichiers dès maintenant.`,
  done: "Terminé",
  refused: (name) => `${name} n’a pas accepté. Rien n’a été partagé.`,
  pairingStopped: (message) => `L’association s’est arrêtée : ${message}`,
  dropOnDesktop: ["Déposer sur le bureau de ", ""],
  nextComputer: "l’ordinateur voisin",
  units: ["o", "Kio", "Mio", "Gio", "Tio"],
};

const MESSAGES: Record<Lang, Messages> = { en, fr };

/** The first of the system's languages that the app speaks, else English. */
function pick(wanted: readonly string[]): Lang {
  for (const tag of wanted) {
    const base = tag.toLowerCase().split("-")[0];
    if (base in MESSAGES) return base as Lang;
  }
  return "en";
}

export const lang: Lang = pick(typeof navigator === "undefined" ? [] : (navigator.languages ?? [navigator.language]));

/** The texts in the user's language. */
export const t: Messages = MESSAGES[lang];

if (typeof document !== "undefined") document.documentElement.lang = lang;

const number = (fractions: number) => new Intl.NumberFormat(lang, { maximumFractionDigits: fractions });

/** A size in binary units (1 KiB = 1024 B), named as such. */
export function bytes(n: number): string {
  let v = n;
  let u = 0;
  while (v >= 1024 && u < t.units.length - 1) {
    v /= 1024;
    u++;
  }
  // A space that does not break between the number and its unit.
  return u === 0 ? `${number(0).format(n)} ${t.units[0]}` : `${number(v < 10 ? 1 : 0).format(v)} ${t.units[u]}`;
}
