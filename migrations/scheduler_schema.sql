--
-- PostgreSQL database dump
--


-- Dumped from database version 16.14
-- Dumped by pg_dump version 16.14

SET statement_timeout = 0;
SET lock_timeout = 0;
SET idle_in_transaction_session_timeout = 0;
SET client_encoding = 'UTF8';
SET standard_conforming_strings = on;
SELECT pg_catalog.set_config('search_path', '', false);
SET check_function_bodies = false;
SET xmloption = content;
SET client_min_messages = warning;
SET row_security = off;

SET default_tablespace = '';

SET default_table_access_method = heap;

--
-- Name: job; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.job (
    id uuid NOT NULL,
    last_updated bigint,
    next_tick bigint,
    last_tick bigint,
    job_type integer NOT NULL,
    count integer,
    ran boolean,
    stopped boolean,
    schedule text,
    repeating boolean,
    repeated_every bigint,
    time_offset_seconds integer,
    extra bytea
);


--
-- Name: notification; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.notification (
    id uuid NOT NULL,
    job_id uuid,
    extra bytea
);


--
-- Name: notification_state; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.notification_state (
    id uuid NOT NULL,
    state integer NOT NULL
);


--
-- Data for Name: job; Type: TABLE DATA; Schema: public; Owner: -
--

COPY public.job (id, last_updated, next_tick, last_tick, job_type, count, ran, stopped, schedule, repeating, repeated_every, time_offset_seconds, extra) FROM stdin;
\.


--
-- Data for Name: notification; Type: TABLE DATA; Schema: public; Owner: -
--

COPY public.notification (id, job_id, extra) FROM stdin;
\.


--
-- Data for Name: notification_state; Type: TABLE DATA; Schema: public; Owner: -
--

COPY public.notification_state (id, state) FROM stdin;
\.


--
-- Name: job pk_metadata; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.job
    ADD CONSTRAINT pk_metadata PRIMARY KEY (id);


--
-- Name: notification pk_notification_id; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.notification
    ADD CONSTRAINT pk_notification_id PRIMARY KEY (id);


--
-- Name: notification_state pk_notification_states; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.notification_state
    ADD CONSTRAINT pk_notification_states PRIMARY KEY (id, state);


--
-- Name: notification_state fk_notification_id; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.notification_state
    ADD CONSTRAINT fk_notification_id FOREIGN KEY (id) REFERENCES public.notification(id) ON DELETE CASCADE;


--
-- PostgreSQL database dump complete
--


