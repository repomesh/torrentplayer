// SPDX-License-Identifier: MIT OR Apache-2.0

import { mount } from 'svelte';
import App from './App.svelte';
import './app.css';

const target = document.getElementById('app');
if (target === null) {
  throw new Error('Missing #app mount point');
}

export default mount(App, { target });
