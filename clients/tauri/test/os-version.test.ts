import { describe, expect, it } from "vitest";

import { osVersionAtLeast, parseOsVersion } from "../src/os-version";
import { fixture } from "./support";

/** `fixtures/os-versions.json`. */
interface OsVersionFixtures {
  parse: { input: string; components: number[] | null }[];
  ordered: string[];
  equal: [string, string][];
  at_least: { version: string; minimum: string; expected: boolean }[];
}

const fixtures = fixture<OsVersionFixtures>("os-versions.json");

function version(text: string): bigint[] {
  const parsed = parseOsVersion(text);
  if (parsed === null) {
    throw new Error(`« ${text} » devrait être lisible`);
  }
  return parsed;
}

describe("versions d'OS", () => {
  it("lit les textes de fixtures/os-versions.json : les nombres lus, ou un texte illisible", () => {
    expect(fixtures.parse.some(({ components }) => components === null)).toBe(true);
    for (const { input, components } of fixtures.parse) {
      expect(parseOsVersion(input)?.map(Number) ?? null, `« ${input} »`).toEqual(components);
    }
  });

  it("refuse un nombre qui ne tient pas sur 64 bits, et pas le plus grand qui y tient", () => {
    expect(parseOsVersion("1.18446744073709551615")).toEqual([1n, 18_446_744_073_709_551_615n]);
    expect(parseOsVersion("1.18446744073709551616")).toBeNull();
  });

  it("suit l'ordre et les égalités de fixtures/os-versions.json", () => {
    const ordered = fixtures.ordered.map(version);
    expect(ordered.length).toBeGreaterThan(1);
    ordered.forEach((lower, index) => {
      for (const higher of ordered.slice(index + 1)) {
        expect(osVersionAtLeast(higher, lower), `${higher.join(".")} ≥ ${lower.join(".")}`).toBe(
          true,
        );
        expect(osVersionAtLeast(lower, higher), `${lower.join(".")} ≥ ${higher.join(".")}`).toBe(
          false,
        );
      }
    });
    expect(fixtures.equal.length).toBeGreaterThan(0);
    for (const [first, second] of fixtures.equal) {
      expect(osVersionAtLeast(version(first), version(second)), `« ${first} » et « ${second} »`).toBe(
        true,
      );
      expect(osVersionAtLeast(version(second), version(first)), `« ${second} » et « ${first} »`).toBe(
        true,
      );
    }
  });

  it("compare à un minimum comme fixtures/os-versions.json", () => {
    expect(new Set(fixtures.at_least.map(({ expected }) => expected))).toEqual(
      new Set([true, false]),
    );
    for (const { version: current, minimum, expected } of fixtures.at_least) {
      expect(
        osVersionAtLeast(version(current), version(minimum)),
        `« ${current} » au moins « ${minimum} »`,
      ).toBe(expected);
    }
  });
});
