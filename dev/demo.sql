-- Demo data for manual testing and screenshots. Loaded on first start of the
-- dev Postgres container (docker-entrypoint-initdb.d).

create schema shop;

create type shop.order_status as enum ('pending', 'paid', 'shipped', 'refunded');

create table shop.customers (
    id bigserial primary key,
    email text not null unique,
    name text not null,
    country char(2) not null,
    created_at timestamptz not null default now(),
    tags text[] not null default '{}'
);

create table shop.products (
    id serial primary key,
    sku text not null unique,
    title text not null,
    price numeric(10, 2) not null check (price >= 0),
    attributes jsonb not null default '{}'
);

create table shop.orders (
    id bigserial primary key,
    customer_id bigint not null references shop.customers (id),
    status shop.order_status not null default 'pending',
    total numeric(12, 2) not null,
    placed_at timestamptz not null,
    shipped_at timestamptz,
    note text
);

create table shop.order_items (
    order_id bigint not null references shop.orders (id),
    product_id int not null references shop.products (id),
    quantity int not null check (quantity > 0),
    unit_price numeric(10, 2) not null,
    primary key (order_id, product_id)
);

create index orders_customer_idx on shop.orders (customer_id);
create index orders_placed_idx on shop.orders (placed_at);

create view shop.order_summary as
select o.id, c.email, o.status, o.total, o.placed_at, count(i.*) as items
from shop.orders o
join shop.customers c on c.id = o.customer_id
left join shop.order_items i on i.order_id = o.id
group by o.id, c.email;

create materialized view shop.daily_revenue as
select date_trunc('day', placed_at)::date as day, sum(total) as revenue, count(*) as orders
from shop.orders
where status <> 'refunded'
group by 1;

create function shop.touch_shipped() returns trigger language plpgsql as $$
begin
    if new.status = 'shipped' and new.shipped_at is null then
        new.shipped_at := now();
    end if;
    return new;
end;
$$;

create trigger orders_shipped before update on shop.orders
for each row execute function shop.touch_shipped();

insert into shop.customers (email, name, country, created_at, tags)
select
    'user' || g || '@example.com',
    initcap(md5(g::text)::varchar(8)) || ' ' || initcap(md5((g * 7)::text)::varchar(10)),
    (array['US', 'GB', 'DE', 'FR', 'JP', 'BR', 'IN', 'CA'])[1 + g % 8],
    now() - (g || ' minutes')::interval,
    case when g % 5 = 0 then array['vip'] when g % 7 = 0 then array['wholesale', 'net30'] else '{}' end
from generate_series(1, 50000) g;

insert into shop.products (sku, title, price, attributes)
select
    'SKU-' || lpad(g::text, 5, '0'),
    (array['Widget', 'Gadget', 'Doohickey', 'Gizmo', 'Sprocket'])[1 + g % 5] || ' ' || g,
    round((random() * 200 + 1)::numeric, 2),
    jsonb_build_object('color', (array['red', 'blue', 'green'])[1 + g % 3], 'weight_g', g % 900 + 100)
from generate_series(1, 2000) g;

insert into shop.orders (customer_id, status, total, placed_at, note)
select
    1 + (g::bigint * 7919) % 50000,
    (array['pending', 'paid', 'shipped', 'refunded']::shop.order_status[])[1 + g % 4],
    round((random() * 500 + 5)::numeric, 2),
    now() - ((g % 525600) || ' minutes')::interval,
    case when g % 11 = 0 then 'gift wrap, please' end
from generate_series(1, 1000000) g;

insert into shop.order_items (order_id, product_id, quantity, unit_price)
select o.id, 1 + (o.id * 31 + k) % 2000, 1 + (o.id + k) % 4, round((random() * 100 + 1)::numeric, 2)
from shop.orders o, generate_series(0, 1) k
where o.id <= 300000;

refresh materialized view shop.daily_revenue;
analyze;
