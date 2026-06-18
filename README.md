# Home Assistant Todo Publisher

A small push-based Rust web service for publishing individual Home Assistant to-do lists without giving consumers any Home Assistant credentials.

Home Assistant pushes one to-do list snapshot to a slug URL such as `/api/todos/cars`. Consumers read the same slug URL with a separate read token.

## Model

Each published to-do list has a URL slug:

```text
cars
shopping
chores
```

The slug is the access-control boundary. A write token for `cars` can only write `/api/todos/cars`; a read token for `cars` can only read `/api/todos/cars`.

There are no namespaces and no aggregate read endpoint.

## API

The service is JSON-only. It has no human-facing HTML page.

### Write one list

```http
POST /api/todos/cars
Authorization: Bearer CARS_WRITE_SECRET
Content-Type: application/json
```

Body:

```json
{
  "id": "todo.cars",
  "name": "Cars",
  "items": [
    {
      "id": "item-1",
      "summary": "Book service",
      "status": "needs_action",
      "due": null,
      "description": null,
      "url": null
    }
  ]
}
```

`updated_at` may be included in the request body, but is optional. If omitted, the server uses the receive time.

The slug must be 1-64 characters of lowercase letters, digits, `-`, or `_`.

Optional stronger write authentication is available with HMAC:

```http
X-HA-Signature-256: sha256=<hex hmac sha256 over raw request body using that slug's write token>
```

If this header is present, it is verified instead of bearer auth. Bearer-token mode is the practical default for Home Assistant YAML because stock templates do not conveniently calculate HMAC.

### Read one list

```http
GET /api/todos/cars
Authorization: Bearer CARS_READ_SECRET
```

Response:

```json
{
  "slug": "cars",
  "updated_at": "2026-01-01T00:00:00Z",
  "list": {
    "id": "todo.cars",
    "name": "Cars",
    "items": [
      {
        "id": "item-1",
        "summary": "Book service",
        "status": "needs_action",
        "due": null,
        "description": null,
        "url": null
      }
    ]
  }
}
```

A valid token for another slug returns `401` for this slug, because the service intentionally treats tokens as per-slug secrets.

### Health

```http
GET /healthz
```

## Configuration

Environment variables:

| Variable | Required | Example | Notes |
| --- | --- | --- | --- |
| `BIND_ADDR` | no | `0.0.0.0:8080` | Listen address. |
| `WRITE_TOKENS` | yes | `cars:write-secret,shopping:write-secret` | Comma-separated `slug:token` entries. Each token min 24 chars. |
| `READ_TOKENS` | yes | `cars:read-secret,shopping:read-secret` | Comma-separated `slug:token` entries. Each token min 24 chars. |

Generate local secrets:

```sh
openssl rand -base64 32
```

Run locally:

```sh
export WRITE_TOKENS="cars:$(openssl rand -base64 32)"
export READ_TOKENS="cars:$(openssl rand -base64 32)"
cargo run
```

## Home Assistant automation

This example publishes a Home Assistant to-do entity called `todo.cars` to the URL slug `cars` whenever it changes. Replace the URL, entity id, and display name for your setup. Store the cars write token in Home Assistant `secrets.yaml`.

`secrets.yaml`:

```yaml
ha_todo_publisher_cars_write_token: "generate-a-long-random-token"
```

`configuration.yaml`:

```yaml
rest_command:
  publish_todos_cars:
    url: "https://todo.example.internal/api/todos/cars"
    method: POST
    content_type: "application/json"
    headers:
      authorization: "Bearer {{ token }}"
    payload: "{{ payload }}"
```

Automation:

```yaml
automation:
  - alias: "Publish cars todo list snapshot"
    mode: restart
    trigger:
      - platform: state
        entity_id: todo.cars
    action:
      - service: todo.get_items
        target:
          entity_id: todo.cars
        data:
          status: needs_action
        response_variable: todo_response
      - service: rest_command.publish_todos_cars
        data:
          token: !secret ha_todo_publisher_cars_write_token
          payload: >-
            {
              "id": "todo.cars",
              "name": "Cars",
              "items": [
                {%- for item in todo_response['todo.cars']['items'] | default([]) -%}
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
```

For another list, add another slug and token pair, for example:

```text
WRITE_TOKENS="cars:<cars-write>,shopping:<shopping-write>"
READ_TOKENS="cars:<cars-read>,shopping:<shopping-read>"
```

Then publish/read `/api/todos/shopping` separately.

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
  WRITE_TOKENS: "cars:replace-with-generated-write-token"
  READ_TOKENS: "cars:replace-with-generated-read-token"
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
- Use separate write and read tokens per slug.
- Rotate a token by replacing its slug entry and restarting the pod.
- The service keeps state in memory only; restart means Home Assistant must push again before data appears.
