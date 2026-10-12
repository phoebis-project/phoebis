import js from '@eslint/js';
import { defineConfig, globalIgnores } from 'eslint/config';
import reactHooks from 'eslint-plugin-react-hooks';
import globals from 'globals';
import tseslint from 'typescript-eslint';

export default defineConfig([
  globalIgnores(['dist/**', 'dist-mock/**', 'node_modules/**', 'smoke/dist/**', 'src/mock/generated/**', 'test/dist/**']),
  js.configs.recommended,
  tseslint.configs.recommended,
  reactHooks.configs.flat.recommended,
  {
    rules: {
      '@typescript-eslint/no-unused-vars': [
        'error',
        {
          argsIgnorePattern: '^_',
          caughtErrorsIgnorePattern: '^_',
          ignoreRestSiblings: true,
          varsIgnorePattern: '^_',
        },
      ],
    },
  },
  { files: ['public/theme.js'], languageOptions: { globals: { ...globals.browser } } },
  { files: ['src/**/*.{ts,tsx}'], languageOptions: { globals: { ...globals.browser } } },
  {
    files: [
      'e2e/**/*.mjs',
      'eslint.config.js',
      'scripts/**/*.mjs',
      'smoke/**/*.{mjs,ts,tsx}',
      'smoke/vite.config.ts',
      'test/**/*.{mjs,ts}',
      'test/vite.config.ts',
      'vite.config.ts',
    ],
    languageOptions: { globals: { ...globals.browser, ...globals.node } },
  },
]);
