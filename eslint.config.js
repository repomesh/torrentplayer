// SPDX-License-Identifier: MIT OR Apache-2.0

import js from '@eslint/js';
import prettier from 'eslint-config-prettier';
import svelte from 'eslint-plugin-svelte';
import globals from 'globals';
import ts from 'typescript-eslint';

export default ts.config(
  {
    ignores: ['dist/', 'docs/book/', 'src-tauri/', 'target/', 'node_modules/'],
  },
  js.configs.recommended,
  ...ts.configs.strictTypeChecked,
  ...svelte.configs.recommended,
  prettier,
  ...svelte.configs.prettier,
  {
    languageOptions: {
      globals: { ...globals.browser },
      parserOptions: {
        projectService: true,
        extraFileExtensions: ['.svelte'],
        tsconfigRootDir: import.meta.dirname,
      },
    },
  },
  {
    files: ['**/*.svelte'],
    languageOptions: {
      parserOptions: { parser: ts.parser },
    },
  },
  {
    files: ['**/*.js'],
    ...ts.configs.disableTypeChecked,
  },
  {
    files: ['scripts/*.mjs'],
    ...ts.configs.disableTypeChecked,
    languageOptions: {
      ...ts.configs.disableTypeChecked.languageOptions,
      globals: globals.node,
    },
  },
);
