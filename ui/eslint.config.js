// SPDX-License-Identifier: MIT OR Apache-2.0

import js from '@eslint/js';
import prettier from 'eslint-config-prettier';
import svelte from 'eslint-plugin-svelte';
import globals from 'globals';
import ts from 'typescript-eslint';

export default ts.config(
  {
    ignores: ['dist/', 'node_modules/'],
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
  // The repository's scripts/ folder, linted by a second run from the root (see "lint" in
  // package.json), where this pattern resolves.
  {
    files: ['scripts/*.mjs'],
    ...ts.configs.disableTypeChecked,
    languageOptions: {
      ...ts.configs.disableTypeChecked.languageOptions,
      globals: globals.node,
    },
  },
);
