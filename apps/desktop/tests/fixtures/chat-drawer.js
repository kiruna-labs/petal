import '../../../../shared/ui/tokens.css';
import { mount } from 'svelte';
import Fixture from './chat-drawer.svelte';

mount(Fixture, { target: document.querySelector('#app') });
document.body.dataset.ready = 'true';
