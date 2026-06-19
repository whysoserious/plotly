# Plotly — instrukcje projektu

## Tryb pracy (WAŻNE — nadpisuje zachowania domyślne)
- **NIE używaj skilla `python-tutor`.** Projekt zmienił kierunek z webowego backendu w Pythonie na **TUI w Rust** sterujące ploterem iDraw 2.0. Tryb tutora jest wyłączony.
- **Piszę kod za użytkownika** — tworzę i edytuję pliki źródłowe bezpośrednio (cargo, moduły, testy). To nie jest projekt „przepisywania snippetów z czatu".
- Źródłem prawdy o projekcie jest **`DESIGN.org`** — czytaj go na początku pracy. (Stary `DESIGN.md` to poprzedni, nieaktualny kierunek — zarchiwizowany.)

## Rytm pracy nad planem (sekcja 14 DESIGN.org) — kontrakt „done"
Implementujemy plan z **sekcji 14** krok po kroku. Dla **każdego** kroku, po napisaniu kodu:
1. **NIE commituję od razu.**
2. **Wypisuję komendy testów do odpalenia przez użytkownika** — komplet do skopiowania: `cargo fmt --check`, `cargo clippy`, `cargo test` (oraz konkretny `cargo test <nazwa>` dla testów /Auto:/ kroku) i `cargo run -- …` z właściwymi flagami.
3. **Dla kroków sprzętowych (✱)** — dodatkowo krótka instrukcja, jak sprawdzić funkcję na fizycznym iDraw (co podłączyć, co wcisnąć, co ma się stać).
4. **Pytam, czy kontynuować** — czekam na potwierdzenie, że testy przeszły i funkcja działa.
5. **Dopiero po „tak"** — robię **commit** (komunikat po angielsku; jeden krok = jeden commit) i zaznaczam `[X]` przy kroku w `DESIGN.org`.

## Konwencja faktów ✔/❓ (NIE mylić!)
- ✔ = potwierdzone w referencyjnym sterowniku (`drawcore_*` / `idraw.py` / `motion.py` / `dripfeed.py`).
- ❓ = **założenie** („czysty Grbl") do weryfikacji na sprzęcie w **spike'u Fazy 0 (krok 0.7)**.
- Komendy realtime (`?`, `!`, `~`, `0x18`, `0x85`, `$J=`) oraz `G21`/`G90` absolutne w mm to **❓** — referencja ich NIE używa (jedzie `G91` względnie, w jednostkach krokowych, z transformem osi i planerem na hoście). **Nie buduj nieodwracalnie na „czystym Grblu" przed spike'em.** Szczegóły: DESIGN.org §2.2/§2.3/§15.

## Konwencje językowe
- **Rozmowa z użytkownikiem:** polski.
- **Wszystko w aplikacji po angielsku:** kod, komentarze, **teksty UI**, komunikaty, **logi**, **komunikaty commitów**. (Użytkownik wprost o to prosił.)
- Aplikacja: **Rust** (edycja 2021), TUI: `ratatui` + `crossterm`. Współbieżność: **wątki + kanały** (bez async/tokio — `serialport` jest blokujący; patrz DESIGN.org §4).
- Użytkownik: 20 lat doświadczenia w innych językach; w Rust kalibruj poziom wyjaśnień w razie potrzeby.

## Sprzęt docelowy (jedyny) — iDraw 2.0
- Użytkownik ma **tylko ploter iDraw 2.0** („iDraw 2.0 Control"). To jedyny target.
- Firmware **DrawCore** — dialekt **G-code w stylu Grbl** (NIE EiBotBoard/EBB jak AxiDraw).
  - USB CH340 — VID:PID `1A86:7523` lub `1A86:8040`. Baud **115200**, 8N1, **RTS=DTR=false** (inaczej reset płytki). Terminator `\r`, odpowiedzi małe `ok` / `error:<n>`.
  - Pen na osi **Z** (down `Z=5` > up `Z=0.5` → większe Z = niżej, ✔). `$H` homing (krańcówki), `$SLP` usypia silniki.
  - Pełna mapa protokołu, ryzyka i konsekwencje (transform osi, bounds = host): DESIGN.org §2.
- **AxiDraw Control = TYLKO inspiracja ficzerów**, nie cel sprzętowy ani protokół (EBB/plotink nas nie dotyczy).

## Build / test
- `cargo build`, `cargo clippy`, `cargo fmt`, `cargo test`.
- Bez sprzętu: `cargo run -- --simulate` (`MockTransport`).
- Logi idą do **pliku** (`./plotly.log`), nie na stdout — TUI zajmuje terminal (DESIGN.org §5).

## Commity
- Po angielsku; jeden krok planu = jeden commit (patrz „Rytm pracy").
- Commituję/pushuję tylko gdy użytkownik o to poprosi (krok planu domyka się jeg
o „tak").

## Materiał referencyjny (read-only)
- Protokół iDrawa: `inkscape extensions/idraw_deps/drawcore_plotink/` (`drawcore_serial.py`, `drawcore_motion.py`) + `idraw_deps/idraw2_0internal/` + `idraw2_0_conf.py`. Dokumentacja firmware DrawCore i algorytmów (CoreXY, transform osi, dripfeed, bounds-clip).
- `inkscape extensions/axidraw_deps/` (plotink/AxiDraw) — tylko pomysły na funkcje, nie protokół.

## Czego nie ruszać bez powodu
- `inkscape extensions/` — materiał referencyjny.
- Stary kod Pythona (`main.py`, `static/`, `pyproject.toml`, `uv.lock`, `.venv/`, `__pycache__/`) i `DESIGN.md` — zarchiwizowany kierunek.
