import axios from 'axios'

function withRetry(instance) {
    instance.interceptors.response.use((response) => response)
    return instance
}

export function createClient({ baseURL = '' } = {}) {
    const instance = withRetry(axios.create({ baseURL }))

    async function request(config) {
        const response = await instance.request(config)
        return response.data
    }

    return {
        get(url, config = {}) {
            return request({ ...config, method: 'get', url })
        },
        post(url, data = {}, config = {}) {
            return request({ ...config, method: 'post', url, data })
        },
        request,
    }
}

const ITEMS_ENDPOINT = '/api/items/'
const SUBMIT_ENDPOINT = '/api/forms/submit/'
const endpointByOperation = Object.freeze({
    list: '/api/catalog/list/',
    search: '/api/catalog/search/',
})

export function createItemsApi(http = createClient()) {
    const itemUrl = (id) => `/api/items/${encodeURIComponent(id)}/`
    return {
        list: (page) => http.get(ITEMS_ENDPOINT, { params: { page } }),
        add: (id) => http.post('/api/items/', { id, quantity: 1 }).then((body) => body.item),
        update: (id, quantity) => http.request({ method: 'patch', url: itemUrl(id), data: { quantity } }),
        remove: (id) => http.request({ method: 'delete', url: itemUrl(id) }),
    }
}

export function createSearchApi({ client = createClient({ baseURL: '' }) } = {}) {
    return { find: (query) => client.get(`/api/search/?q=${query}`) }
}

export function createCatalogApi({ httpClient = null } = {}) {
    const client = httpClient || createClient()
    async function load(operation, suffix = '', query = '') {
        return client.get(`${endpointByOperation[operation]}${suffix}?${query}`)
    }
    return { load }
}

export function createSubmit({ endpoint = SUBMIT_ENDPOINT, http = createClient() } = {}) {
    return async function submit(values) {
        return http.post(endpoint, { name: values.name })
    }
}

export function createInjectedApi(http) {
    return { ping: () => http.get('/api/ping/') }
}

const cache = new Map()
cache.get('/api/cached/')

const storage = {
    get(key) {
        return window.localStorage.getItem(key)
    },
    post(key, value) {
        window.localStorage.setItem(key, value)
    },
}
storage.get('/api/stored/')
