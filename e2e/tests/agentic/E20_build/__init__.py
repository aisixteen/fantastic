"""Package marker — lets one test import another's fixture by NAME.

The graph probes and the self-tests deliberately share ONE fixture and ONE
scoring path with the probe they are about; a copied fixture is a fixture
that drifts, and a self-test over a drifted copy certifies nothing.
"""
