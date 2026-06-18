# Home Assistant Todo Publisher

A small push-based Rust web service for publishing Home Assistant to-do lists without giving an external service a broad Home Assistant long-lived access token.

Home Assistant owns the data and pushes snapshots to this service whenever a configured to-do list changes. Consumers then read a simple JSON API using separate read tokens.

## Why push instead of polling Home Assistant?

Home Assistant long-lived access tokens are too broad for this use case. This service avoids storing any Home Assistant credential. Instead:

- Home Assistant sends `POST /api/ingest` with a service-specific write token.
- Readers call `GET /api/todos` or `GET /api/todos/{namespace}` with separate read tokens.
- The app stores only the latest in-memory snapshot per namespace.

No personal to-do contents are committed to this repository.

## API

### Write: ingest a namespace snapshot

```http
POST /api/ingest
Authorization: Bearer <write-token>
Content-Type: application/json
```

Body:

```json
{
  "namespace": "home",
  "lists": [
    {
      "id": "todo.example",
      "name": "Example list",
      "items": [
        {
          "id": "item-1",
          "summary": "Example task",
          "status": "needs_action",
          "due": null,
          "description": null,
          "url": null
        }
      ]
    }
  ]
}
```

The namespace must be 1-64 characters of lowercase letters, digits, `-`, or `_`.

Optional stronger write authentication is available with HMAC:

```http
X-HA-Signature-256: sha256=<hex hmac sha256 over raw request body using WRITE_TOKEN>
```

If this header is present, it is verified instead of bearer auth. The bearer-token mode is the practical default for Home Assistant YAML because stock templates do not conveniently calculate HMAC.

### Read all namespaces

```http
GET /api/todos
Authorization: Bearer <read-token>
```

### Read one namespace

```http
GET /api/todos/home
Authorization: Bearer <read-token>
```

### Health

```http
GET /healthz
```

## Configuration

Environment variables:

| Variable | Required | Example | Notes |
| --- | --- | --- | --- |
| `BIND_ADDR` | no | `0.0.0.0:8080` | Listen address. |
| `WRITE_TOKEN` | yes | generated secret | Minimum 24 chars. Used only by Home Assistant writes. |
| `READ_TOKENS` | yes | `trmnl:secret1,dashboard:secret2` | Comma-separated `name:token` entries. Each token min 24 chars. |
| `PUBLIC_HTML` | no | `false` | If `true`, `/` is public. JSON remains token-protected. |

Generate local secrets:

```sh
openssl rand -base64 32
```

Run locally:

```sh
export WRITE_TOKEN="$(openssl rand -base64 32)"
export READ_TOKENS="local:$(openssl rand -base64 32)"
cargo run
```

## Home Assistant automation

This example publishes a single Home Assistant to-do entity into namespace `home` whenever that entity changes. Replace the URL, entity id, and display name for your setup. Store the write token in Home Assistant `secrets.yaml`.

`secrets.yaml`:

```yaml
ha_todo_publisher_write_token: "generate-a-long-random-token"
```

`configuration.yaml`:

```yaml
rest_command:
  publish_todos_home:
    url: "https://todo.example.internal/api/ingest"
    method: POST
    content_type: "application/json"
    headers:
      authorization: "Bearer {{ token }}"
    payload: "{{ payload }}"
```

Automation:

```yaml
automation:
  - alias: "Publish home todo list snapshot"
    mode: restart
    trigger:
      - platform: state
        entity_id: todo.example
    action:
      - service: todo.get_items
        target:
          entity_id: todo.example
        data:
          status: needs_action
        response_variable: todo_response
      - service: rest_command.publish_todos_home
        data:
          token: !secret ha_todo_publisher_write_token
          payload: >-
            {
              "namespace": "home",
              "lists": [
                {
                  "id": "todo.example",
                  "name": "Example list",
                  "items": [
                    {%- for item in todo_response['todo.example']['items'] | default([]) -%}
                    {
                      "id": {{ (item.uid | default(item.summary, true)) | to_json }},
                      "summary": {{ item.summary | to_json }},
                      "status": {{ (item.status | default('needs_action', true)) | to_json }},
                      "due": {{ (item.due | default(none)) | to_json }},
                      "description": {{ (item.description | default(none)) | to_json }},
                      "url": null
                    }{{ "," if not loop.last else "" }}
                    {%- endfor -%}
                  ]
                }
              ]
            }
```

For multiple independent uses, create separate namespaces such as `home`, `shopping`, `dashboard`, or `trmnl`. A namespace upload replaces only that namespace, leaving the others intact.

## Kubernetes sketch

Kubernetes manifests are provided under `deploy/`:

- `namespace.yaml`
- `secret.example.yaml`
- `deployment.yaml`
- `service.yaml`
- `ingress.example.yaml`

The ingress example intentionally uses placeholder hostnames and TLS secret names. Replace them with your local cluster values.

```yaml
apiVersion: v1
kind: Secret
metadata:
  name: ha-todo-publisher-secrets
type: Opaque
stringData:
  WRITE_TOKEN: "replace-with-generated-secret"
  READ_TOKENS: "dashboard:replace-with-generated-secret,trmnl:replace-with-generated-secret"
---
apiVersion: apps/v1
kind: Deployment
metadata:
  name: ha-todo-publisher
spec:
  replicas: 1
  selector:
    matchLabels:
      app: ha-todo-publisher
  template:
    metadata:
      labels:
        app: ha-todo-publisher
    spec:
      containers:
        - name: app
          image: ghcr.io/clawdy00/ha-todo-publisher:latest
          ports:
            - containerPort: 8080
          envFrom:
            - secretRef:
                name: ha-todo-publisher-secrets
          readinessProbe:
            httpGet:
              path: /healthz
              port: 8080
          livenessProbe:
            httpGet:
              path: /healthz
              port: 8080
```

Add a Service and Ingress for your local cluster hostname.

## Security notes

- Do not commit real to-do payloads, tokens, screenshots, or Home Assistant hostnames if private.
- Use one write token for Home Assistant and separate read tokens per consumer.
- Rotate a read token by replacing it in `READ_TOKENS` and restarting the pod.
- The service keeps state in memory only; restart means Home Assistant must push again before data appears.
