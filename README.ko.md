[English](README.md) | 한국어

# gitcask

gitcask는 S3 호환 오브젝트 스토리지를 유일한 영속 계층으로 사용하는 무상태 git 서버입니다. 각 저장소는 버킷에 write-ahead log 형태로 저장되고, 서버는 캐시만 보관합니다. 별도의 데이터베이스나 리더 선출이 없으며, 운영 비용이 저장소 수가 아니라 push 수에 비례하므로 유휴 저장소를 유지하는 데는 비용이 들지 않습니다.

유저 프로젝트마다 저장소를 하나씩, 프로그램으로 만들고 지우는 플랫폼을 위해 설계되었습니다.

- **HTTP를 통한 git** — clone, fetch, push, LFS를 표준 git 클라이언트로 그대로 사용할 수 있습니다.
- **JSON API** — 트리·커밋·diff 조회, 파일 커밋, 브랜치 생성, 머지를 clone이나 작업 디렉토리 없이 수행합니다.
- **웹훅** — 모든 ref 변경을 영속 커서 기반으로 한 번씩 전달하며, 재전송도 가능합니다.
- **무상태 서버** — 어떤 인스턴스든 모든 저장소를 서빙할 수 있고, 새 인스턴스는 몇 초 안에 refs 서빙을 시작합니다.
- **인증 내장** — 플랫폼이 자기 키로 서명한 JWT를 gitcask가 공개키로 검증합니다. 유저 데이터베이스는 필요하지 않습니다.

## 어디서 왔나

Vicent Martí가 이 아키텍처를 Cursor의 [*Git at any scale*](https://cursor.com/blog/git-at-any-scale)에서 설명했습니다. Cursor는 이를 Continuity라 부르며 대형 모노레포를 운영하는 데 사용하고 있습니다. Tobi Lütke가 이를 [walgit](https://github.com/tobi/walgit)으로 재현했고, gitcask는 walgit의 포크입니다. Cursor는 이 설계가 "양방향으로 스케일한다"고 적었습니다 — 거대한 저장소 하나로도, 많은 수의 작은 저장소로도. gitcask는 그중 후자를 위해 만들어졌습니다: 플랫폼이 만들고 지우는 다수의 작은 저장소입니다. walgit에서 무엇이 왜 달라졌는지는 [docs/DIRECTION.md](docs/DIRECTION.md)에 기록되어 있습니다.

gitcask가 존재할 수 있는 것은 Cursor가 설계를 공개하고 Tobi가 코드를 공개해 준 덕분입니다. 두 분께 감사드립니다.

## 5분 안에 돌려보기

```sh
docker compose up --build --wait
TOKEN=$(docker compose run --rm token --config /etc/gitcask/gitcask.standalone.toml token mint \
  --key /run/secrets/gitcask-private.pem --principal local-demo --scope local/demo:admin --ttl 1h)
curl -fsS -X PUT -H "Authorization: Bearer $TOKEN" http://127.0.0.1:8080/local/demo
git clone "http://ignored:$TOKEN@127.0.0.1:8080/local/demo.git"
cd demo && git commit --allow-empty -m first && git push -u origin HEAD:main
```

로컬 S3 호환 스토리지(rustfs)와 gitcask 프로세스 하나가 실행됩니다. 위 토큰은 쓰고 버리는 키로 서명한 데모용 JWT입니다. 프로덕션에서는 플랫폼이 자기 Ed25519 개인키로 토큰을 서명하고, gitcask는 공개키만 보관합니다. git은 토큰을 Basic 인증의 비밀번호 자리에 실어 보내며, 사용자 이름은 무시됩니다.

토큰 수명에 대한 참고: 사람이 git에 입력하는 토큰은 OS 자격 증명 저장소에 저장되어 재사용됩니다. 몇 시간이 아니라 몇 주처럼 오래 살아야 한다면 그만큼 scope를 좁히세요 — 저장소 하나, 최소 권한. 요청마다 발급하는 백엔드 토큰은 분 단위로 만료해도 됩니다.

## 어떻게 동작하나

저장소는 버킷의 `repos/<owner>/<repo>/` 아래에 write-ahead log로 저장됩니다. push는 불변 팩과 로그 엔트리를 업로드한 뒤 작은 manifest를 compare-and-swap으로 갱신합니다. 이 CAS가 유일한 합의 지점이며, 리더 선출이나 쿼럼은 사용하지 않습니다. 두 인스턴스가 동시에 갱신을 시도하면 한쪽만 성공하고 다른 쪽은 재시도합니다.

읽기는 manifest 조건부 GET으로 시작합니다. 대부분은 304가 돌아와 로컬 사본을 사용하고, 변경이 있으면 새 엔트리를 적용한 뒤 응답합니다. 따라서 한 인스턴스에서 완료 응답을 받은 push는 다른 모든 인스턴스에서도 곧바로 조회됩니다.

서버가 모두 사라져도 데이터는 버킷에 남아 있습니다. 새 인스턴스를 버킷에 연결하면 몇 초 안에 refs를 서빙하고, 첫 객체 요청 때 팩을 내려받습니다. 체크포인트, 컴팩션, 무결성 감사, 가비지 컬렉션 같은 유지보수 작업은 push가 남기는 마커를 기준으로 실행되므로, 최근 push가 없는 저장소는 방문하지 않습니다.

전체 설계와 각 결정의 근거, 왕복 비용 모델은 [AGENTS.md](AGENTS.md)에 정리되어 있습니다.

## 범위

다음은 의도적으로 호출하는 플랫폼의 몫으로 남겨져 있습니다:

- **저장소 목록과 검색.** 목록 조회에는 버킷 스캔이 필요한데, 이는 이 설계가 피하는 연산입니다. 권위 있는 목록은 플랫폼의 데이터베이스에서 관리하는 것을 전제로 합니다.
- **유저와 로그인.** gitcask는 불투명한 principal 문자열을 받아 서명을 검증할 뿐입니다. 신원·권한·팀은 플랫폼에 남으므로 동기화할 계정 데이터가 없습니다.
- **CI, 이슈, UI.** 웹훅으로 어떤 CI 시스템이든 연동할 수 있고, API 위에 어떤 UI든 얹을 수 있습니다. 경계와 그 근거는 [docs/PRODUCT.md](docs/PRODUCT.md)에 있습니다.

데이터는 언제든 반출할 수 있습니다. `git clone --mirror`로 저장소를 내보낼 수 있고(LFS 콘텐츠는 `git lfs fetch --all` 추가), 일관된 시점의 버킷 복사본 — 쓰기를 멈춘 상태에서 복사하거나, S3 버전닝·시점 복제를 이용한 것 — 은 완전한 백업입니다: 새 배포를 연결하면 그대로 서빙됩니다.

## 실행

```sh
# 빌드 (rust-toolchain.toml에 지정된 Rust 버전과 protoc 필요)
cargo build --release -p gitcask-cli
# 또는: docker build -t gitcask -f Containerfile .

# 한 대 구성: 인증을 켠 gitcask를 8080 포트에서, 로컬 rustfs를 스토리지로
docker compose up --build -d
curl http://127.0.0.1:8080/healthz
```

- [`gitcask.standalone.toml`](gitcask.standalone.toml) — 단일 프로세스 구성. 시작점으로 적합합니다.
- [`gitcask.example.toml`](gitcask.example.toml) — 모든 설정 키와 기본값, 주석.
- [`deploy/nginx.conf.example`](deploy/nginx.conf.example) — 필요 시 앞단에 두는 nginx. 공개 TLS와 바이트 오프로드용.

역할(`server.roles`): `serve`(git, API, LFS), `maintain`(체크포인트·컴팩션·fsck), `events`(웹훅 브리지). 빈 목록이면 세 역할을 모두 수행합니다. 하나의 버킷을 여러 호스트가 공유할 수 있습니다.

## 개발

```sh
just test          # 빠른 격리 테스트 단계 (< 1분)
just e2e           # 실행 중인 서버를 상대로 한 실제 git 테스트 (~20초)
just ci            # 머지에 필요한 전부: warnings, clippy, test, e2e
scripts/smoke.sh . 8090                    # 로컬 rustfs를 상대로 한 엔드투엔드 테스트
cargo test -p gitcask-server --test sim    # 장애 주입: 크래시, 파티션, 오래된 값 읽기
```

```
crates/
  gitcask-proto    protobuf 스키마(wal.proto), 로그 프레이밍, 스토어 키
  gitcask-store    ObjectStore trait, S3 백엔드, lease, 재시도
  gitcask-git      디스크의 bare 저장소, receive-pack, 팩 인제스트, refs
  gitcask-wal      sync 레벨, publish(group commit + CAS), 체크포인트, tasks
  gitcask-server   axum: smart HTTP, LFS, 인증, JSON API, maintainer, 이벤트 브리지
  gitcask-config   gitcask.toml(GITCASK__ 환경 변수로 재정의), fail-closed 검증
  gitcask-cli      gitcask serve|import|migrate|compact|wal|repo|token
```

더 자세한 문서: [docs/PRODUCT.md](docs/PRODUCT.md)(제품 경계), [docs/DIRECTION.md](docs/DIRECTION.md)(이 포크의 결정), [docs/OPERATIONS.md](docs/OPERATIONS.md)(운영 절차서), [docs/MIGRATION.md](docs/MIGRATION.md)(Gitea에서 이전하기), [docs/ROUNDTRIPS.md](docs/ROUNDTRIPS.md)(비용 모델), [docs/EVENTS.md](docs/EVENTS.md), [docs/INTEGRITY.md](docs/INTEGRITY.md), [docs/LFS.md](docs/LFS.md).

## 기여

개발·테스트·DCO 요건은 [CONTRIBUTING.md](CONTRIBUTING.md)에 있습니다. 취약점은 [SECURITY.md](SECURITY.md)의 안내에 따라 비공개로 신고해 주세요. 참여는 [Contributor Covenant](CODE_OF_CONDUCT.md)를 따릅니다.
