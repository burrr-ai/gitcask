import type { Metadata } from 'next'

import { Figure, Flow } from '@/lib/components/diagram'
import { Callout } from '@/lib/components/docs/callout'
import { CodeBlock } from '@/lib/components/docs/code-block'
import { Definitions } from '@/lib/components/docs/definitions'
import { DocArticle } from '@/lib/components/docs/doc-article'
import { Section } from '@/lib/components/docs/section'
import { SpecTable } from '@/lib/components/docs/spec-table'

export const metadata: Metadata = { title: 'Authentication' }

const SECTIONS = [
  { id: 'modes', title: 'Five modes' },
  { id: 'tokens', title: 'Tokens and scopes' },
  { id: 'introspection', title: 'Introspection' },
  { id: 'proxy', title: 'Trusted proxies' },
  { id: 'errors', title: 'Errors and open paths' },
  { id: 'self-hosting', title: 'Keys for self-hosters' },
]

const JWT_CONFIG = `[server]
auth_mode = "jwt"

[auth.jwt]
public_key = "/etc/gitcask/public.pem"   # or jwks_url = "https://issuer.example/.well-known/jwks.json"
issuer = "https://issuer.example"        # exact iss; required in jwt mode
# audience = "gitcask"                   # optional exact member of aud
leeway = "60s"                           # exp/iat/nbf clock skew`

const INTROSPECT_ANSWER = `{"active":true,"principal":"user:42","scopes":["acme/*:read"],"ttl":30}`

const TOKEN_COMMANDS = `# once: an Ed25519 key pair (never overwrites existing files)
gitcask --config gitcask.toml token keygen \\
  --private-key gitcask-private.pem --public-key gitcask-public.pem

# per token: issuer and audience come from [auth.jwt] in the config
gitcask --config gitcask.toml token mint \\
  --key gitcask-private.pem --principal ci --scope acme/web:write --ttl 1h`

export default function AuthenticationPage() {
  return (
    <DocArticle
      slug="authentication"
      title="Authentication"
      lede="gitcask verifies a credential on every request and checks repository scopes; issuing tokens and storing users stay with your platform."
      sections={SECTIONS}
      sources={['AGENTS.md', 'SECURITY.md', 'docs/PRODUCT.md', 'gitcask.example.toml']}
    >
      <Section id="modes" title="Five modes">
        <p>
          Pick one with <code>server.auth_mode</code>. Startup fails closed when a mode&apos;s settings are missing.
        </p>
        <SpecTable
          columns={['Mode', 'Needs', 'Caller']}
          rows={[
            [
              <code key="m">none</code>,
              <>
                <code>server.listen</code> on loopback; refused otherwise
              </>,
              <>
                Everyone is <code>anon</code> with write and admin
              </>,
            ],
            [
              <code key="m">jwt</code>,
              <>
                <code>[auth.jwt]</code>: one of <code>public_key</code> or <code>jwks_url</code>, plus{' '}
                <code>issuer</code>
              </>,
              <>
                The token&apos;s <code>sub</code> and <code>scopes</code>
              </>,
            ],
            [
              <code key="m">introspect</code>,
              <>
                <code>[auth.introspect]</code>: <code>url</code> and <code>secret_env</code>
              </>,
              <>
                The issuer&apos;s <code>principal</code> and <code>scopes</code>
              </>,
            ],
            [
              <code key="m">forwarded</code>,
              <>
                A trusted proxy; <code>GITCASK_FORWARD_SECRET</code> is optional and must match when set
              </>,
              <>
                <code>X-Gitcask-Principal</code>, with <code>X-Gitcask-Write: 1</code> and{' '}
                <code>X-Gitcask-Admin: 1</code> as grants
              </>,
            ],
            [
              <code key="m">introspect_forwarded</code>,
              <>
                <code>[auth.introspect]</code> and <code>GITCASK_FORWARD_SECRET</code>, both required at startup
              </>,
              'One identity per request: proxy or token, never both',
            ],
          ]}
        />
        <CodeBlock code={JWT_CONFIG} lang="toml" title="gitcask.toml" />
      </Section>

      <Section id="tokens" title="Tokens and scopes">
        <p>Git sends the token as the Basic-auth password and ignores the username. The API takes the same token as a Bearer header.</p>
        <Figure
          label="git sends the token in the Basic password; API clients send it as Authorization: Bearer."
          className="not-prose grid gap-3 rounded-card bg-muted p-6 md:p-8"
        >
          <Flow
            steps={[
              { label: 'git', tone: 'quiet' },
              { label: 'Basic, password = $TOKEN', code: true },
            ]}
          />
          <Flow
            steps={[
              { label: 'API client', tone: 'quiet' },
              { label: 'Bearer $TOKEN', code: true },
            ]}
          />
        </Figure>
        <p>
          JWTs are EdDSA (Ed25519) only. gitcask holds the public key; the issuer keeps the private key.
        </p>
        <Definitions
          code
          items={[
            { term: 'sub', children: 'Opaque principal. gitcask stores nothing else about the caller.' },
            {
              term: 'scopes',
              children: (
                <>
                  <code>&lt;owner&gt;/&lt;repo&gt;:read|write|admin</code>. <code>*</code> is allowed only in the
                  repository segment; admin implies write implies read.
                </>
              ),
            },
            { term: 'exp, iat, jti', children: 'Required.' },
            {
              term: 'iss',
              children: (
                <>
                  Must equal <code>auth.jwt.issuer</code>.
                </>
              ),
            },
            {
              term: 'aud',
              children: (
                <>
                  Checked when <code>auth.jwt.audience</code> is set.
                </>
              ),
            },
            { term: 'nbf', children: 'Honoured when present.' },
          ]}
        />
      </Section>

      <Section id="introspection" title="Introspection">
        <p>For opaque tokens, gitcask asks your issuer and caches the answer per instance.</p>
        <div className="not-prose">
          <Flow
            steps={[
              { label: 'Basic password or Bearer', tone: 'quiet' },
              { label: 'POST url {"token":"…"}', tone: 'brand', code: true },
              { label: 'active, principal, scopes, ttl', code: true },
            ]}
          />
        </div>
        <CodeBlock code={INTROSPECT_ANSWER} lang="json" title="200 answer" />
        <SpecTable
          columns={['Key or limit', 'Default', 'Bound']}
          rows={[
            [<code key="k">cache_ttl</code>, <code key="v">30s</code>, <>≤ 10m; <code>0s</code> disables positive caching</>],
            [<code key="k">negative_cache_ttl</code>, <code key="v">3s</code>, <><code>0s</code> disables negative caching</>],
            [<code key="k">timeout</code>, <code key="v">2s</code>, '> 0s and ≤ 10s'],
            ['Cached answers', '10,000 per instance, FIFO', 'Keyed by SHA-256, never raw tokens'],
            ['Answer body', '64 KiB', <>Larger answers are a service failure: 503</>],
          ]}
        />
        <p>
          A positive answer lives <code>min(ttl, cache_ttl)</code>, which is also the longest a revocation can take to
          reach an instance.
        </p>
      </Section>

      <Section id="proxy" title="Trusted proxies">
        <p>
          In <code>introspect_forwarded</code>, the <code>X-Gitcask-Forward-Secret</code> header decides who the caller is.
        </p>
        <SpecTable
          columns={['X-Gitcask-Forward-Secret', 'Authentication and precedence']}
          rows={[
            [
              'Absent',
              'Introspect the Basic password or Bearer token using the existing bounded client/cache. Ignore all forwarded identity/write/admin headers.',
            ],
            [
              'Present but empty, malformed, repeated, or wrong',
              <>
                Return 401; never retry with <code>Authorization</code>, even if it contains a valid token.
              </>,
            ],
            [
              'Present once and valid',
              <>
                Compare in constant time, then require a non-empty <code>X-Gitcask-Principal</code>. Use only forwarded
                identity/grants; ignore <code>Authorization</code>, even if valid, invalid or more privileged. A
                missing/empty principal returns 401.
              </>,
            ],
          ]}
        />
        <Callout tone="warning" title="Strip client headers at the proxy">
          <p>
            The proxy must remove every client-supplied <code>X-Gitcask-Principal</code>, <code>X-Gitcask-Write</code>,{' '}
            <code>X-Gitcask-Admin</code> and <code>X-Gitcask-Forward-Secret</code>, including duplicates, before adding
            its own. Forwarded grants are not repository scopes: the proxy authorizes the repository.
          </p>
        </Callout>
      </Section>

      <Section id="errors" title="Errors and open paths">
        <SpecTable
          columns={['Situation', 'Answer']}
          rows={[
            [
              'Missing or invalid credentials',
              <>
                401 with <code>WWW-Authenticate: Basic realm=&quot;gitcask&quot;</code>
              </>,
            ],
            ['Token lacks a scope for the repository', '404'],
            ['Forwarded principal missing or empty', '401'],
            [
              <>
                Forwarded request without <code>X-Gitcask-Write: 1</code> or <code>X-Gitcask-Admin: 1</code> it needs
              </>,
              '403',
            ],
            [
              'Introspection service failure',
              <>
                503 with <code>Retry-After: 5</code>, no challenge
              </>,
            ],
          ]}
        />
        <SpecTable
          columns={['Path', 'Credentials']}
          rows={[
            [
              <>
                <code>/healthz</code>, <code>/readyz</code>
              </>,
              'Open',
            ],
            [
              <>
                <code>/docs</code>, <code>/openapi.json</code>, <code>/api/v1/docs</code>, <code>/api/v1/openapi.json</code>
              </>,
              <>
                Open while <code>server.public_docs = true</code> (default); required when <code>false</code>
              </>,
            ],
            [<code key="p">/metrics</code>, 'Always required'],
            ['Everything else', 'Required'],
          ]}
        />
      </Section>

      <Section id="self-hosting" title="Keys for self-hosters">
        <p>
          Without an issuer, generate a key pair and mint tokens offline. gitcask has no endpoint that issues tokens.
        </p>
        <CodeBlock code={TOKEN_COMMANDS} />
      </Section>
    </DocArticle>
  )
}
