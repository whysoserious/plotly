# Plotly — backend do sterowania ploterem SVG

## 1. Cel aplikacji
Aplikacja webowa do sterowania ploterem rysującym pliki SVG. Użytkownik wgrywa plik SVG przez przeglądarkę, podgląda go w GUI, docelowo wysyła do plotera. Backend wystawia REST API; frontend jest minimalny i serwowany z tego samego procesu. Całość pakowana w obraz Docker, dystrybuowana przez Docker Hub — odpalalna jednym `docker compose up` u każdego.

## 2. Funkcje (MVP)
- [ ] `POST /api/files` — upload pliku SVG (multipart/form-data), zapis w pamięci serwera, zwrot `{id, filename, size}`.
- [ ] `GET /api/files/{id}/preview?n=500` — zwraca pierwsze N znaków treści SVG jako plain text.
- [ ] `GET /api/files/{id}/content` — zwraca pełną treść SVG z `Content-Type: image/svg+xml`.
- [ ] `GET /api/files` — lista wgranych plików.
- [ ] Serwowanie statycznego frontendu (`/`) — input file + guzik upload + podgląd SVG + textarea z preview.
- [ ] Dockerfile + obraz uruchamialny przez `docker run`.
- [ ] `docker-compose.yml` jako preferowany sposób uruchamiania (zwłaszcza gdy dojdzie ploter).

## 3. Funkcje (poza MVP, na później)
- [ ] **Komunikacja z ploterem przez TCP** — aplikacja łączy się z `PLOTTER_HOST:PLOTTER_PORT` (config przez env). Lokalnie na macOS host uruchamia `socat` mostkujący `/dev/tty.usbserial-*` → TCP. Na Linuxie/Pi to samo z `socat` lub `ser2net`. Aplikacja nie wie czy ploter jest fizyczny, sieciowy czy zmockowany.
- [ ] Parsowanie SVG i ekstrakcja metadanych (rozmiar, viewBox, liczba ścieżek).
- [ ] Konwersja SVG → komendy plotera (G-code / HPGL — do ustalenia od modelu plotera).
- [ ] `POST /api/plot/{id}` — wysyłanie pliku na ploter.
- [ ] Status plotera (busy/idle, postęp) — `GET /api/plot/status`, SSE / WebSocket.
- [ ] Kolejka zleceń.
- [ ] Persystencja plików (dysk / SQLite).
- [ ] Autoryzacja (na razie open — działa lokalnie).
- [ ] Publikacja na Docker Hub przez GitHub Actions.

## 4. Stack technologiczny
| Warstwa | Biblioteka | Dlaczego ta? |
|---|---|---|
| Język | Python 3.14 | Najnowszy stabilny — t-strings (PEP 750), ulepszone typing, opcjonalny free-threaded mode. |
| Web framework | **FastAPI** | Type-hint-first, walidacja Pydantic, automatyczne `/docs` (Swagger UI). |
| ASGI server | **Uvicorn** | Standardowy runtime FastAPI. W kontenerze `uvicorn main:app`. |
| Multipart upload | **python-multipart** | Wymagane przez FastAPI do `UploadFile`. |
| Plotter comms (poza MVP) | **stdlib `asyncio.open_connection`** | TCP socket — bez dodatkowej zależności. Ploter abstrakcjonowany jako endpoint sieciowy. |
| Host-side bridge USB→TCP | **`socat`** (na hoście, nie w obrazie) | Nie zależność Pythona — narzędzie systemowe. Linuxowy odpowiednik: `ser2net`. |
| Frontend | Vanilla HTML + JS (fetch) | Brak build-stepu — backend serwuje statyczny plik. |
| Pakowanie | **Docker** + `python:3.12-slim` | Mały obraz, szybki start, dystrybucja przez `docker compose up`. |
| Zależności | **uv** | Szybkie (Rust), trzyma lockfile, jedno narzędzie zamiast `python`/`pip`/`venv`. Standard nowych projektów. |
| Linter/formatter | **ruff** | Zastępuje flake8 + black + isort, ekstremalnie szybki. |

## 5. Architektura

```
┌─────────────────────────────────────────────────────┐
│  Browser                                            │
│  ┌─────────────┐  upload  ┌────────────────────┐    │
│  │ index.html  │ ───────► │ POST /api/files    │    │
│  │  + app.js   │ ◄─────── │ {id, filename,size}│    │
│  └─────────────┘  display └────────────────────┘    │
│         │ GET /api/files/{id}/content (svg+xml)     │
│         └──► osadzone w <object>                    │
└─────────────────────────────────────────────────────┘
                          │ HTTP :8000
                          ▼
┌─────────────────────────────────────────────────────┐
│  Docker container                                   │
│  Uvicorn + FastAPI (jeden proces)                   │
│                                                     │
│  ┌─────────────┐   ┌──────────────────┐             │
│  │ src/api.py  │ ► │ src/storage.py   │             │
│  │ (routery)   │   │ (in-memory dict) │             │
│  └─────────────┘   └──────────────────┘             │
│         │ (poza MVP)                                │
│         ▼                                           │
│  ┌──────────────────┐                               │
│  │ src/plotter.py   │ ── TCP ──┐                    │
│  │ (asyncio client) │          │                    │
│  └──────────────────┘          │                    │
│                                │                    │
│  StaticFiles("/static") na "/" │                    │
└────────────────────────────────┼────────────────────┘
                                 │ host.docker.internal:5000
                                 ▼
┌─────────────────────────────────────────────────────┐
│  HOST (macOS / Linux)                               │
│  ┌──────────────────────────────────────────────┐   │
│  │ socat TCP-LISTEN:5000 ⇄ /dev/tty.usbserial-* │   │
│  └──────────────────────────────────────────────┘   │
│                          │                          │
│                          ▼                          │
│                    [ PLOTER ]                       │
└─────────────────────────────────────────────────────┘
```

**Decyzja kluczowa:** aplikacja nigdy nie dotyka `/dev/tty*` bezpośrednio. Komunikuje się z ploterem **tylko przez TCP** (`PLOTTER_HOST:PLOTTER_PORT` z env). Mostkowanie USB → TCP to odpowiedzialność hosta (`socat`/`ser2net`). Dzięki temu:
- działa identycznie na macOS i Linuxie,
- nie wymaga `--device=` w Dockerze,
- ploter może być fizycznie na innej maszynie (RPi przy ploterze, backend gdziekolwiek),
- łatwo wstawić mock plotera do testów.

## 6. Struktura plików
```
plotly/
├── main.py                  # entry point: tworzy FastAPI, montuje routery i statykę
├── src/
│   ├── __init__.py
│   ├── api.py               # endpointy /api/files/*
│   ├── storage.py           # in-memory storage uploadów
│   └── plotter.py           # (poza MVP) klient TCP do plotera
├── static/
│   ├── index.html
│   └── app.js
├── tests/
│   └── test_api.py
├── Dockerfile
├── .dockerignore
├── docker-compose.yml       # docelowy sposób uruchomienia (z konfigem env)
├── pyproject.toml
└── README.md
```

## 7. Dystrybucja i uruchamianie

**Dla użytkownika końcowego — dwie ścieżki:**

1. **Szybkie demo (bez plotera):**
   ```bash
   docker run -p 8000:8000 twojlogin/plotly
   ```

2. **Docelowo (z ploterem):** użytkownik bierze `docker-compose.yml`, edytuje jedną zmienną (port socat), odpala:
   ```bash
   docker compose up
   ```
   `docker-compose.yml` ma sekcję:
   ```yaml
   environment:
     - PLOTTER_HOST=host.docker.internal
     - PLOTTER_PORT=5000
   ```
   Plus instrukcja w README, jak uruchomić `socat` na hoście (osobna komenda — celowo nie automatyzujemy w obrazie, bo `socat` musi widzieć device hosta).

## 8. Plan pracy (kroki nauki)

- [x] **Krok 1: Setup środowiska + Hello World FastAPI**
  - [x] Instalacja `uv`
  - [x] `uv init --python 3.12` → finalnie `requires-python = ">=3.14"` (wybór: 3.14)
  - [x] `uv add fastapi 'uvicorn[standard]'`
  - [x] `main.py` z `GET /` → `{"hello": "world"}`
  - [x] Uruchomienie `uv run uvicorn main:app --reload`, podgląd `/docs`
  - **Czego się uczymy:** venv, pip/uv, struktura projektu, ASGI vs WSGI, dekorator `@app.get`, automatyczne docs.

- [x] **Krok 2: Upload pliku (POST /api/files)**
  - [x] Endpoint z `UploadFile` (+ doinstalowanie `python-multipart`)
  - [x] In-memory storage z UUID, dataclass `StoredFile`
  - [x] Walidacja MIME / rozszerzenie + limit 10 MB
  - [x] Pydantic response model `FileMetadata` + `response_model=...`
  - **Czego się uczymy:** `UploadFile`, async w FastAPI, Pydantic models, type hinty w sygnaturach, podstawy dependency injection.

- [ ] **Krok 3: Endpointy odczytu (preview, content, list)**
  - [ ] `GET /api/files`
  - [ ] `GET /api/files/{id}/preview?n=500`
  - [ ] `GET /api/files/{id}/content` z `Content-Type: image/svg+xml`
  - [ ] `HTTPException` na 404
  - **Czego się uczymy:** path/query params, custom `Response`, HTTPException, status codes.

- [ ] **Krok 4: Frontend (statyczny)**
  - [ ] Mount `StaticFiles` na `/`
  - [ ] `index.html` + `app.js` — input file, fetch, `<object>` + `<pre>`
  - **Czego się uczymy:** `StaticFiles`, kolejność montowania, CORS (i czemu tu nie potrzebujemy).

- [ ] **Krok 5: Refaktor do `src/`**
  - [ ] `src/api.py` jako `APIRouter`
  - [ ] `src/storage.py` jako klasa
  - [ ] `main.py` tylko składa
  - **Czego się uczymy:** `APIRouter`, struktura modułów, `__init__.py`, importy względne vs absolutne.

- [ ] **Krok 6: Testy (pytest + httpx)**
  - [ ] Setup `pytest`
  - [ ] Testy happy path + 404 + walidacja
  - **Czego się uczymy:** pytest, fixtures, `TestClient`, organizacja testów.

- [ ] **Krok 7: Dockerfile**
  - [ ] `python:3.12-slim`, kopia kodu, instalacja zależności, `EXPOSE 8000`, `CMD`
  - [ ] `.dockerignore`
  - [ ] Test `docker build` + `docker run -p 8000:8000`
  - **Czego się uczymy:** warstwy Dockera, kolejność `COPY` dla cache, host binding (0.0.0.0).

- [ ] **Krok 8: docker-compose.yml + publikacja na Docker Hub**
  - [ ] `docker-compose.yml` z env vars (`PLOTTER_HOST`, `PLOTTER_PORT`)
  - [ ] `docker login`, tagowanie (`username/plotly:0.1.0` + `latest`), `docker push`
  - [ ] README z instrukcją uruchomienia
  - **Czego się uczymy:** docker-compose syntax, env vars w kontenerze, Docker Hub, tagowanie.

- [ ] **Krok 9 (poza MVP): Klient TCP do plotera**
  - [ ] `src/plotter.py` z `asyncio.open_connection`
  - [ ] Endpoint `POST /api/plot/{id}` wysyłający dane przez socket
  - [ ] Instrukcja `socat` w README
  - [ ] Mock plotera (server TCP w testach)
  - **Czego się uczymy:** `asyncio` streams, lifecycle połączeń, integracja z FastAPI, mockowanie w testach.

## 9. Czego user się nauczył (dziennik)

### Krok 1 (setup + hello world)
- **Ekosystem narzędzi:** `uv` jako menedżer pakietów (lockfile `uv.lock`, `.venv/`, `uv run` bez aktywacji venv). `uv add` modyfikuje `pyproject.toml`. Extras w cudzysłowach (`'uvicorn[standard]'`) bo bash globbing.
- **Pliki projektu:** `pyproject.toml` (PEP 621), rozróżnienie `requires-python` (constraint dla konsumentów pakietu) vs `.python-version` (lokalny runtime). `[build-system]` z `hatchling`.
- **Python vs CPython:** Język vs implementacja. GIL, refcounting, C-API to detale CPythona. PyPy/Jython/GraalPy istnieją ale ekosystem żyje na CPythonie. `uv` pobiera CPython z `python-build-standalone`.
- **Składnia Pythona:** Import (`from X import Y` vs `import X`), instancja klasy bez `new`, dekoratory (`@app.get(...)` jako wywołanie zwracające wrapper).
- **Type hinty:** `dict[str, str]` lowercase (PEP 585, od 3.9). Nie wymuszane w runtime — sprawdzają je `mypy`/`pyright`/`ty`. FastAPI używa ich do walidacji.
- **FastAPI/ASGI:** `app = FastAPI()` to obiekt ASGI. `async def` vs `def` — FastAPI sam decyduje co odpalić w event loop, co w thread pool. Pułapka: sync I/O w `async def` blokuje loop.
- **Uvicorn:** `uv run uvicorn main:app --reload`. Składnia `module:attribute`. `--reload` dzięki `watchfiles` z `[standard]`. `127.0.0.1` lokalnie, `0.0.0.0` będzie konieczne w Dockerze.
- **Auto-docs:** `/docs` (Swagger UI), `/redoc` (ReDoc), `/openapi.json` (źródło prawdy). Generowane z type hintów i dekoratorów.

### Krok 2 (upload pliku)
- **Pydantic vs dataclass:** `BaseModel` na granicy systemu (waliduje + serializuje + schema OpenAPI), `@dataclass` wewnątrz (lekkie, bez walidacji). Generalna zasada: Pydantic na zewnątrz, dataclass w środku.
- **`from __future__ import annotations`** — lazy evaluation type hintów (PEP 563). Idiom nowoczesnego Pythona, na 3.14 niedługo default (PEP 649/749).
- **Mutable class default pitfall:** atrybut klasy `_files: dict = {}` jest **wspólny dla wszystkich instancji**. Inicjalizuj w `__init__`. Brak ekwiwalentu tej pułapki w Javie/Kotlinie.
- **Union types z `|`** (PEP 604, od 3.10): `StoredFile | None` zamiast `Optional[StoredFile]`. Lowercase generics (`dict[...]`, `list[...]`) od 3.9 (PEP 585).
- **`UploadFile`** — FastAPI rozpoznaje typ parametru i generuje multipart upload w `/docs`. Wymaga `python-multipart` (osobna zależność, FastAPI nie ciągnie).
- **`@app.post(response_model=...)`** — filtrowanie pól zwracanych do klienta. Nawet jeśli funkcja zwróci więcej, wyjdzie tylko to co w modelu (mechanizm bezpieczeństwa).
- **`async def` + `await file.read()`** — `UploadFile.read()` jest async, czyta ze spooled buffera. Reguła: sync I/O w `async def` = blokujesz event loop.
- **HTTPException** z `fastapi.status` (stałe nazwane). `raise HTTPException(status_code=..., detail=...)` zamiast `ValueError`.
- **`a or b` jako fallback** — idiom dla falsy values (None, "", 0). Analogiczne do `??` / `?:`.
- **Konwencja `_prefix`** — funkcja "prywatna" modułu/klasy. Python tego nie wymusza; sygnał dla czytelnika i lintera.
- **Czytanie tracebacku:** od dołu do góry. Ostatnia linia = typ wyjątku + komunikat (często z rozwiązaniem); głębsze ramki = gdzie wybuchło; górne = nasza wejściówka. Pythonowe wyjątki są gadatliwe — czytaj komunikat.
- **Recovery venv:** uszkodzona biblioteka? `rm -rf .venv uv.lock && uv sync` to kanoniczna "reinstalacja" — analogia do `npm ci`.

## 10. Otwarte pytania / decyzje do podjęcia
- **Limit rozmiaru uploadu:** Czy ograniczamy (np. 10 MB)? Domyślnie Starlette nie limituje. Decyzja w Kroku 2.
- **Persystencja:** In-memory dla MVP. Przeskoczymy na dysk/SQLite gdy zaczniemy sterować ploterem (długie zlecenia muszą przetrwać restart).
- **Marka i protokół plotera:** Jaki ploter masz / planujesz? G-code, HPGL, własny protokół? To wpłynie na Krok 9.
- **Skąd brać `host.docker.internal` na Linuxie:** Na macOS działa "z pudełka", na Linuxie wymaga `--add-host=host.docker.internal:host-gateway` w compose. Doprecyzujemy w Kroku 8.
