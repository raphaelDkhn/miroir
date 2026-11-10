FROM node:22.22.2-bookworm-slim
WORKDIR /app
COPY package.json package-lock.json ./
RUN npm ci --omit=dev --ignore-scripts --no-audit --no-fund && npm cache clean --force
COPY src/ dist/
COPY openapi.yaml ./
RUN mkdir -p uploads
EXPOSE 3000
CMD ["node", "dist/server.js"]
