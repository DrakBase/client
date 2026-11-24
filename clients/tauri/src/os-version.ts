/**
 * Version d'un système, comme `os_version` ou `min_os_version` : des nombres séparés par des
 * points (docs/PROTOCOLE.md, « Versions d'OS »). Mêmes règles que `am_protocol::OsVersion` côté
 * Rust et `SystemVersion` côté Swift, sur les cas de `fixtures/os-versions.json`.
 */

/** Au-delà, un texte est illisible. */
const MAX_BYTES = 64;
/** Un nombre tient sur 64 bits, ou le texte est illisible. */
const MAX_COMPONENT = 0xffff_ffff_ffff_ffffn;

/**
 * Lecture souple, celle du serveur : les blancs autour sont retirés, puis les nombres de tête
 * comptent. Elle s'arrête au premier composant qui n'est pas fait que de chiffres, dont elle garde
 * les chiffres de tête : `6.8.0-45-generic` vaut `6.8.0`, et `2..1` vaut `2`. Les zéros finaux ne
 * sont pas gardés.
 *
 * `null` pour un texte illisible : celui qui ne commence pas par un chiffre (`v14`, `.5`, un texte
 * vide), qui dépasse 64 octets, ou dont un nombre ne tient pas sur 64 bits.
 */
export function parseOsVersion(text: string): bigint[] | null {
  const trimmed = text.trim();
  if (new TextEncoder().encode(trimmed).length > MAX_BYTES) {
    return null;
  }
  const components: bigint[] = [];
  for (const part of trimmed.split(".")) {
    const digits = /^[0-9]*/.exec(part)?.[0] ?? "";
    if (digits === "") {
      break;
    }
    const value = BigInt(digits);
    if (value > MAX_COMPONENT) {
      return null;
    }
    components.push(value);
    if (digits.length !== part.length) {
      break;
    }
  }
  while (components.length > 1 && components.at(-1) === 0n) {
    components.pop();
  }
  return components.length === 0 ? null : components;
}

/**
 * Vrai si `version` vaut au moins `minimum` : composant par composant, un composant absent valant
 * 0. `14.6.1` > `14.6` > `14.0`, et `14` égale `14.0.0`.
 */
export function osVersionAtLeast(version: readonly bigint[], minimum: readonly bigint[]): boolean {
  const length = Math.max(version.length, minimum.length);
  for (let index = 0; index < length; index += 1) {
    const left = version[index] ?? 0n;
    const right = minimum[index] ?? 0n;
    if (left !== right) {
      return left > right;
    }
  }
  return true;
}
