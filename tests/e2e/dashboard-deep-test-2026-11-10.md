# Dyp test av dashboardet, 10. november 2026

Testet: main (de83f15) med PR #147 (Auto Setup) og #148 (toast) slått sammen, i en sky-container uten Discord. Discord er erstattet av `tests/e2e/mock_discord.py`, nå med sju brukere og to servere. Alt er kjørt med Playwright (webapp-testing) i headless Chromium, release-bygg av UI.

Kjør selv: `DEEP=1 tests/e2e/run.sh` (ca. 1 time), eller én del: `DEEP_ONLY=matrix node tests/e2e/deep.js` etter `psql -f tests/e2e/levels.sql`. Delene heter `matrix`, `pages`, `owner`, `extras` og `contrast`. `DEEP_PAGES=automod,joinraid` begrenser til noen sider.

## Hva som er testet

| Del | Hva | Omfang |
|---|---|---|
| matrix | Hver skrive-rute i API-et, for hvert tilgangsnivå, uten og med 2FA. Forventet svar regnes ut fra nivå, 2FA og permit. | 51 ruter × 7 brukere × 2 = 714 kall |
| matrix | Endring på den andre serveren, der brukerne ikke har tilgang | 7 brukere |
| pages | Alle 35 sider som hver bruker: «Ingen tilgang», infoboks og grå felter stemmer med det serveren sier, menyen skjuler skjulte sider, ingen kildekode som tekst, ingen konsollfeil. På skrivebeskyttede sider trykkes hver knapp, bryter og felt, og ingen skriving får lov. | 245 sidevisninger |
| owner | Eier på alle sider: hvert tallfelt (+ og −, og tall utenfor grensen), første bryter (linjen «Changes detected», Lagre, last inn på nytt, verdien er lagret), Avbryt setter alt tilbake | alle sider med felter |
| extras | Serverbytte i venstremenyen og «refresh server», 2FA-spørsmål ved sensitiv lagring, Auto Setup fra veiviseren, toast (lukkes ved klikk, feiltoast), lys/mørk og at valget huskes, mobil 390 px (ingen sideveis scrolling på noen side, menyknappen åpner og lukker) | |
| contrast | Musen over hver lenke og knapp på alle sider, i lys og mørk modus. Kontrast under 3:1 er feil. | 35 sider + Update, Commands, serverlisten, × 2 tema |

Brukerne (se `tests/e2e/levels.sql`): Discord-eier, Extra Owner, Trusted Admin, editor (permit `dashboard_edit`), viewer (`dashboard_view`), saksbehandler (`dashboard_view`, `cases`, `notes_view`) og et medlem uten permit.

## Resultat

**Ingen kan endre noe de ikke har lov til.** Alle 714 kall i matrisen ga riktig svar. TA kan ikke endre TA-listen, Extra Owner kan ikke endre Extra Owner-listen eller rescue-nøkkelen, og ingen andre enn eieren kan skrive til den andre serveren.

### Funnet og rettet

| # | Funn | Hvor | Rettelse |
|---|---|---|---|
| 1 | Valgt lenke i venstremenyen (serveren på «Servers», og åpen side inne på en server) ble nesten usynlig under musen: mørk tekst på mørk bakgrunn, 1,1:1. Thomas' funn. | `style.css` | Valgt-regelen har nå samme spesifisitet som hover-regelen. Kontrasttesten finner ingen andre steder under 3:1. |
| 2 | Join Raid kunne ikke slås på i en server som aldri hadde lagret Join Raid: «missing field `suspicious_only`». | `automod/src/join.rs` | `RaidCfg` fikk `#[serde(default)]`, som de andre seksjonene. |
| 3 | Et tømt tallfelt lagret `null`, og lagringen ble avvist med «invalid type: null, expected i64». Gjaldt 57 tallfelt. I Panic Mode var «Quarantined accounts to trip» tomt fra start, så +/− og lagring feilet der. | `form.rs`, `schema.rs` | Tomt tallfelt får feltets standardverdi. «Quarantined accounts to trip» viser 0 (av). |
| 4 | Desimalfelt fikk avrundingsfeil med +/−: 0,01 + 1 − 1 ga 0,010000000000000009, og skjemaet så endret ut. | `form.rs` | Desimalsteg rundes av som prosentfelt. |
| 5 | Hvert sidebytte lastet innstillinger, skjema og Discord-data to ganger, fordi siden ble bygget på nytt når tilgangen ble hentet igjen. Sider ble også bygget før tilgangen var kjent, så skjulte sider sendte forespørsler. | `main.rs` | Tilgangen leses gjennom en `Memo`, og siden bygges først når tilgangen for riktig server er kjent. |
| 6 | Kopier-knappen sa «Copied!» også når nettleseren nektet, og feilen ble en ufanget sidefeil. Gjaldt også Rescue-nøkkelen, som bare vises én gang. | `pages.rs`, `common.rs` | Knappen venter på svaret og sier «Copy failed, select and copy by hand» når kopiering nektes. |
| 7 | Lockdown var fullt åpen for viewers: Lock-knappene virket trykkbare, men serveren nektet. | `access.rs` | Lockdown er skrivebeskyttet med infoboks under editor. |
| 8 | «Roll back» på Historikk var aktiv for alle, og «Import settings» for viewers. | `main.rs` | Knappene er grå uten nok tilgang (Roll back: Trusted Admin, Import: editor). |
| 9 | Valgt tema (lys/mørk) ble glemt ved ny innlasting. | `main.rs` | Valget lagres i nettleseren. |
| 11 | Import-knappen på listefelt (Strict Words, Wildcard Words og alle andre lister med Export/Import) ble fet og strakk seg over hele raden, og hjelpeteksten lå oppå den. Thomas' funn i Wizard 6. | `style.css`, `form.rs` | Import er en vanlig knapp, og hjelpeteksten står under feltet. |
| 10 | Lagring av en seksjon og import sjekket innholdet før tilgangen, så en uten tilgang fikk vite hvordan seksjonen ser ut fra feilmeldingen. | `api.rs` | Tilgangen sjekkes først. |

Runde 2: hver feil over er lett etter flere steder, i koden og med nye sjekker.

| # | Funn | Hvor | Rettelse |
|---|---|---|---|
| 12 | Rettelse 11 virket ikke: `.io label` tapte mot `.row label`, som står senere i fila. Import var fortsatt bred og fet. Thomas' funn. | `style.css` | `.row label:has(> input)` vinner på spesifisitet. |
| 13 | Samme regel gjorde alle avkrysningsbokser i rolle- og kanallister fete og med stor forbokstav (Main Roles, Partnering Channels og 8 andre sider). | `style.css` | Samme rettelse. |
| 14 | `@everyone` sto som «@@everyone» i rollelister og i forhåndsvisning av meldinger. | `form.rs`, `md.rs` | Ingen ekstra @ når navnet har den. |
| 15 | Hjelpetekst ved brytere ble klemt i to smale kolonner (Join Gate «Exclude bots added by the owner», to på Miscellaneous, Auto Mod, Verification). | `style.css` | Hjelpeteksten står på egen linje under. |
| 16 | På mobil (390 px) var toppmenyen bredere enn skjermen: avataren ble kuttet og søkefeltet dekket logoen. | `style.css`, `main.rs` | Under 600 px ligger Commands og Update i kontomenyen. |
| 17 | Oversikt: Fix-knappene og systembryterne var aktive for viewers (serveren nektet). Samme feil som 7 og 8. | `pages.rs` | Grå uten tilgang. |
| 18 | Oversikt: etter én bryter ble neste bryter avvist med «config changed, reload», fordi hver bryter hadde sin egen versjon. Fem like forespørsler ved lasting. | `pages.rs` | Én felles config og versjon. |
| 19 | Join Raid «Margin» (`id_margin_ms`) kunne ikke tømmes: lagringen feilet med null. Samme feil som 3, det siste feltet. | `schema.rs` | Standard 1 time, som boten. Ny test over alle tallfelt. |
| 20 | + og så − på et tomt valgfritt felt ga 0 i stedet for tomt. For Join Raid «ID flag» betyr 0 at alle som blir med under et raid flagges. | `form.rs` | Ned til 0 på et felt uten standard gir tomt igjen. |
| 21 | Et desimalfelt viste «100.0» etter +/−. | `form.rs` | Desimaler vises som tall. |

Info-ikonet (godkjent av Thomas): ikonet `info` fra emoji-settet med tooltip-kortet fra designspesifikasjonen. Det står ved 21 vanskelige innstillinger (lista i `schema.rs`, `TIPS`), og erstatter de røde «! Notes» på kort som ikke er i bruk med «Not in use right now». Kortet holder seg inne på siden ved kantene, og layout-testen åpner hvert kort.

Nye tester: `schema::tests::number_fields_without_default_accept_null` (alle tallfelt), `one_field_alone_deserializes` (alle seksjoner lastes med bare ett felt, fanger feil 2 igjen), og `DEEP_ONLY=overflow` (alle sider i 1400, 1024 og 390 px: tekst utenfor boksen, kuttet, oppå annen tekst eller klemt). Tilgang før validering, kopiering og manglende `serde(default)` er sjekket i alle ruter og strukturer uten flere funn.

Nye feiltyper er lagt i `PITFALLS.md` (rot) og `crates/dashboard/ui/PITFALLS.md` (punkt 3 til 7).

### Sett, ikke rettet (trenger ditt valg eller er med vilje)

- **Tallfelt uten grenser.** Bare antinuke-grensene og Join Gate-alder har min/maks. Alle andre tallfelt tar imot alt, for eksempel 999 999 999 minutter strike-timeout. Boten kapper timeouts til 28 dager, så ingenting går i stykker, men dashboardet viser en verdi som ikke gjelder. Forslag: grenser i `core::limits` for timeouts (maks 28 dager) og antallfelt. Det er en større runde, fordi hvert felt trenger sin grense.
- **Feilmelding fra en handling lukker seg selv** (for eksempel «user ID must be a number» på Notes). Bare feil fra lagring av innstillinger blir stående, slik PITFALLS sier. Si fra hvis handlinger også skal bli stående.
- **Bryterne på Oversikt** lagrer med en gang uten «Changes detected». Det er slik de er laget.
- **Logging-siden** henter config to ganger (notisen om egne kanaler og skjemaet). Ufarlig.
- **2FA-vinduet** har ikke `role="dialog"`, så skjermlesere sier ikke at det er et vindu.

### Ikke dekket

Discord-gatewayen og boten selv (mocken svarer bare på REST-kallene dashboardet gjør), ekte TOTP-koder (2FA er satt direkte i databasen), og Safari/Firefox (bare Chromium).
