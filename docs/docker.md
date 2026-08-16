# Запуск GitAI в Docker

Подробное руководство по быстрому и удобному запуску **GitAI** с помощью Docker и Docker Compose.

---

## 🚀 Быстрый старт за 1 минуту

### 1. Подготовка конфигурации
Скопируйте пример файла с переменными окружения:
```bash
cp .env.example .env
```
Откройте файл `.env` в любом редакторе и укажите API-ключ вашей нейросети (например, `ANTHROPIC_API_KEY`, `OPENAI_API_KEY` или `OPENROUTER_API_KEY`).

### 2. Запуск контейнера
Запустите GitAI в фоновом режиме:
```bash
docker compose up -d
```

### 3. Проверка статуса
```bash
# Проверка логов
docker compose logs -f gitai

# Проверка работоспособности через встроенный doctor
docker compose exec gitai gitai doctor
```

GitAI будет доступен по адресу: `http://localhost:8080` (healthcheck: `http://localhost:8080/healthz`).

---

## ⚙️ Структура данных и персистентность

При запуске автоматически создаётся локальная папка `./data`:
- `data/gitai.toml` — файл конфигурации GitAI. При первом запуске создаётся автоматически, если его не было.
- `data/gitai.db` — база данных SQLite с задачами, попытками и событиями.
- `data/prompts/` — шаблоны промптов (planner, worker, editor, reviewer, arbiter).
- `data/work/` — рабочие копии репозиториев для выполнения задач.

---

## 🛠 Полезные команды

### Выполнение проверки конфигурации (`doctor`)
```bash
docker compose exec gitai gitai doctor
```

### Ручной запуск одной задачи (`run`)
Вы можете запустить автономный цикл решения задачи для любого репозитория:
```bash
docker compose exec gitai gitai run \
  --repo /data/work/my-repo \
  --title "Fix memory leak in buffer pool" \
  --body "Buffer was not deallocated on premature socket close."
```

### Запуск интерактивного шелла внутри контейнера
```bash
docker compose exec -it gitai /bin/bash
```

### Остановка и перезапуск
```bash
# Остановка
docker compose down

# Перезапуск с пересборкой
docker compose up -d --build
```

---

## 📦 Дополнительные сценарии

### Запуск полного стека (GitAI + локальный Gitea)
В `docker-compose.yml` встроен профиль `full`, который сразу запускает и настраивает локальный сервер Gitea:
```bash
docker compose --profile full up -d
```
Gitea будет доступен на `http://localhost:3000`.

### Использование локального Ollama
Если у вас запущен Ollama на хост-машине (порт 11434):
1. В `.env` укажите:
   ```env
   OLLAMA_BASE_URL=http://host.docker.internal:11434/v1
   PLANNER_PROVIDER=ollama
   PLANNER_MODEL=qwen2.5-coder:14b
   WORKER_PROVIDER=ollama
   WORKER_MODEL_A=qwen2.5-coder:14b
   WORKER_MODEL_B=deepseek-coder-v2:16b
   ```
2. Перезапустите:
   ```bash
   docker compose up -d
   ```

### Режимы изоляции (Sandbox)
- **`SANDBOX_KIND=docker`** (по умолчанию): GitAI использует сокет хоста `/var/run/docker.sock` для запуска изолированных контейнеров для каждого воркера.
- **`SANDBOX_KIND=local`**: команды компиляции и тестов выполняются напрямую внутри контейнера GitAI (подходит для простой разработки и отладки).
