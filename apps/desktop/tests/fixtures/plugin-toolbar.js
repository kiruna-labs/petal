import '../../../../shared/ui/tokens.css';
import '../../../../shared/ui/plugin-provenance.css';
import { mount } from 'svelte';
import Fixture from './plugin-toolbar.svelte';

window.__events = [];
mount(Fixture, { target: document.querySelector('#app') });
document.body.dataset.ready = 'true';
