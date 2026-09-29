import { mount } from 'svelte';
import Fixture from './keyed-tile-flip.svelte';

mount(Fixture, { target: document.querySelector('#app') });
document.body.dataset.ready = 'true';
