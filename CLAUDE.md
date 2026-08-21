# relax-ng

Rust-Crate: eigenständige, spec-treue RELAX-NG-Implementierung (Schema
parsen, komponieren, gegen ein Dokument validieren). Konzept & Herkunft:
`README.md`. Umsetzungsplan: `plan/`.

Schwester-Projekt von [`html-conform`](../html-conform) — wird dort als
Abhängigkeit für die RelaxNG-Validierungsschicht (`schema.rs`, Phase 05)
eingebunden, sobald veröffentlicht. Steht aber für sich: generisch,
keine HTML-/XML-Parser-Abhängigkeit, kein Wissen über HTML-Konformität.

## Architektur (Arbeitstitel, siehe `plan/` für Details)

```
RELAX-NG-Schema (.rnc/.rng) → Syntax-Layer (AST)
                             → Komposition/Simplification (include/div/combine → ein Grammar-Modell)
                             → Validierungs-Engine (Pattern-Matching gegen ein generisches Document/Node-Trait)
```

Wer ein Dokument validieren will, bringt seinen eigenen Baum mit (über ein
Trait) — dieses Crate parst/baut keine XML- oder HTML-Bäume selbst.

## Arbeitsweise

- Aktueller Stand & nächster Schritt: `plan/00-STATUS.md`.
- Phasenpläne mit Schritten/Exit-Kriterien: `plan/0N-*.md`. Vor größeren
  Änderungen die passende Phase lesen, nicht am Plan vorbei arbeiten.
- Getroffene Entscheidungen: `plan/DECISIONS.md` — dort nachschlagen,
  bevor offene Fragen neu aufgerollt werden.

## Feste Regeln

- Lizenz: **MIT**, von Anfang an (`Cargo.toml`: `license = "MIT"`) — kein
  Grauzonen-Zeitraum ohne Lizenz.
- **Kein Code aus `dholroyd/relaxng-rust` übernehmen** — dieses Projekt ist
  unlizenziert, wir dürfen nur seine öffentlich beschriebene Architektur
  (Syntax/Modell/Validator-Aufteilung) als Inspiration nutzen, nicht seinen
  Quelltext lesen-und-übertragen. Bei Unsicherheit: eigenständig aus der
  RELAX-NG-Spec (https://relaxng.org/spec-20011203.html) herleiten.
- Die offizielle RELAX-NG-Testsuite (Phase 02) ist vendorter Fremdcode wie
  in `html-conform`s `schema/`/`tests/corpus/` — Herkunft/Lizenz vor dem
  Vendoring klären und dokumentieren, nicht von Hand editieren.
- Kein HTML-, XML-Parser- oder sonstige Host-Format-Abhängigkeit im Kern —
  Instanzdokumente kommen ausschließlich über ein generisches Trait rein.
- Kein `unsafe` ohne expliziten Grund und Kommentar.

## Definition of Done

Siehe "Exit-Kriterien" in der jeweiligen `plan/0N-*.md`-Datei — nicht
global definiert, sondern pro Phase.
