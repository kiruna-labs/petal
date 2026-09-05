import './fidelityFixture.css';
import { mountFidelityFixture } from './fidelityFixture';

const root = document.querySelector<HTMLElement>('#fidelity-app');
if (root) mountFidelityFixture(root);
