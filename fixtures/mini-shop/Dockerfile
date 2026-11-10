FROM node:22.22.2-bookworm-slim AS app
WORKDIR /app
COPY package.json package-lock.json ./
RUN npm ci --omit=dev --ignore-scripts --no-audit --no-fund
COPY src/ dist/
COPY openapi.json ./

FROM mysql:8.4.13
COPY --from=app /usr/local/bin/node /usr/local/bin/node
COPY --from=app /app /app
WORKDIR /app
EXPOSE 3001
CMD ["node", "dist/server.js"]
